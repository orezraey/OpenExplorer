//! A work or school account's recycle bin, which Microsoft Graph doesn't reach: Graph v1.0
//! restores only on personal accounts (`driveItem: restore`) and lists a recycle bin only for
//! SharePoint Embedded containers. A work or school OneDrive is a SharePoint site
//! (`https://contoso-my.sharepoint.com/personal/ana_contoso_com`), and SharePoint's own REST API
//! lists its recycle bin, restores from it and deletes from it for good:
//!
//! - `GET {site}/_api/site/RecycleBin`: both stages (the site's bin and the site collection's,
//!   where what's deleted from the first goes), each item with its `ItemState` (1 first stage,
//!   2 second). The person owns their OneDrive's site collection, so it's theirs to see; if
//!   SharePoint refuses it, `_api/web/RecycleBin` (the first stage only) is used, and so are
//!   its items' addresses.
//! - `POST {site}/_api/site/RecycleBin('{id}')/restore()` and `…/deleteObject()`: one item.
//!   Esvaziar deletes exactly the items listed, one by one, rather than `DeleteAll()`, which
//!   would also take what arrived since the list was read.
//!
//! The token is SharePoint's, not Graph's: asked with the same refresh token for
//! `https://{host}/.default` (the host from the drive's `webUrl`, the only one it's sent to), which
//! holds whatever SharePoint permissions the app registration lists and the person (or an
//! administrator) granted. Until then the token is refused ([`crate::graph::NEEDS_CONSENT`]): the
//! account goes on working through Graph, and the Lixeira says what to add to the registration
//! and offers a sign-in that grants it ([`crate::oauth::grant`]). A bearer token needs no
//! `X-RequestDigest`; if SharePoint asks for one anyway, it's read from `_api/contextinfo` once.
//!
//! Desfazer after Excluir: Graph deletes (in one `$batch`, as on personal accounts), then the bin
//! is read for what just arrived and each item is matched by its name, its folder (`DirName`,
//! compared from the library on, so the site's address doesn't matter) and the time it was
//! deleted. The match is kept by the item's Graph id, so the app's undo steps stay the same for
//! both kinds of account.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock, Mutex};

use openexplorer_i18n::{t, tr};
use serde_json::Value;

use crate::account::{Account, Area, Item, Kind};
use crate::graph::{Body, Fail, NEEDS_CONSENT, Res, segment};
use crate::{PendingLogin, Result, log};

/// How many SharePoint requests go at once when restoring or deleting several items.
const AT_ONCE: usize = 4;
/// How far the clocks of this machine and SharePoint's may disagree, in seconds.
const SKEW: i64 = 300;

/// An item in the recycle bin, as the Lixeira shows it.
#[derive(Clone, Debug, PartialEq)]
pub struct BinItem {
    /// SharePoint's id of the recycle bin item (a GUID).
    pub id: String,
    /// The name shown: the item's own, or with " (2)" and so on when several in the bin share it.
    pub name: String,
    /// The item's own name (`LeafName`).
    pub leaf: String,
    /// Where it was deleted from, as SharePoint says it (`DirName`).
    pub dir: String,
    /// Where it was deleted from, as the panes show it ("Meus arquivos/Fotos").
    pub origin: String,
    /// When it was deleted, in seconds since 1970.
    pub deleted: i64,
    pub size: u64,
    pub is_dir: bool,
    /// In the site collection's recycle bin (the second stage).
    pub second_stage: bool,
}

/// Whether the account's recycle bin can be reached.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Access {
    /// Not asked yet.
    #[default]
    Unknown,
    Ready,
    /// SharePoint's permission isn't granted (or not in the registration): Microsoft's words.
    NeedsConsent(String),
}

/// What a Lixeira shows with no account open (the app's scenes), by drive id.
static STAGED: LazyLock<Mutex<HashMap<String, Listed>>> = LazyLock::new(Mutex::default);

/// Whether a Lixeira can be reached, and what it listed last.
type Listed = (Access, Arc<Vec<BinItem>>);

/// What account `drive`'s Lixeira listed last (restoring and deleting go by the names shown,
/// and the app shows where each item came from), and whether it can be reached.
fn seen(drive: &str) -> Listed {
    match crate::account::cached(drive) {
        Some(a) => lock(&a.bin.shown).clone(),
        None => STAGED.lock().unwrap_or_else(|e| e.into_inner()).get(drive).cloned().unwrap_or((Access::Unknown, Arc::default())),
    }
}

/// The items account `drive`'s Lixeira listed last.
pub fn shown(drive: &str) -> Arc<Vec<BinItem>> {
    seen(drive).1
}

/// The listed item shown as `name`.
pub fn shown_item(drive: &str, name: &str) -> Option<BinItem> {
    shown(drive).iter().find(|b| b.name == name).cloned()
}

pub fn access(drive: &str) -> Access {
    seen(drive).0
}

/// Sets what a Lixeira shows without an account (the app's scenes).
pub fn show(drive: &str, items: Vec<BinItem>, access: Access) {
    STAGED.lock().unwrap_or_else(|e| e.into_inner()).insert(drive.to_string(), (access, Arc::new(items)));
}

/// What can't be done in the Lixeira.
pub fn fixed() -> &'static str {
    t(
        "A Lixeira do OneDrive não recebe itens nem abre o que tem: use Restaurar ou Excluir permanentemente.",
        "The OneDrive recycle bin doesn't take items or open what it holds: use Restore or Delete forever.",
    )
}

/// What the person has to do for the Lixeira of a work or school account.
pub fn consent_help() -> String {
    t(
        "A Lixeira de uma conta corporativa ou de estudante é lida pela API do SharePoint, que precisa de uma permissão a mais no registro do aplicativo: no portal do Microsoft Entra, em Permissões de API › Adicionar uma permissão › SharePoint › Permissões delegadas, marque AllSites.Write. Depois clique em Autorizar no navegador (ou peça a um administrador que conceda o consentimento para a organização). Sem isso a conta continua funcionando; só a Lixeira e o Desfazer da exclusão ficam de fora.",
        "A work or school account's recycle bin is read through SharePoint's API, which needs one more permission in the app registration: in the Microsoft Entra portal, under API permissions › Add a permission › SharePoint › Delegated permissions, check AllSites.Write. Then click Authorize in browser (or ask an administrator to grant consent for the organization). Without it the account keeps working; only the recycle bin and undoing deletes are left out.",
    )
    .into()
}

/// The site the drive is, and how to reach its REST API.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Site {
    /// The site's address, no trailing slash.
    pub url: String,
    /// `https://{host}/.default`: SharePoint's token for this host.
    pub scope: String,
    /// The document library's path on the host, decoded (`/personal/ana_contoso_com/Documents`).
    pub library: String,
}

impl Site {
    /// From the drive root's `webUrl` (the library) and `sharepointIds.siteUrl`. The site must be
    /// on the library's host: the token goes only there.
    pub fn of(web_url: &str, site_url: Option<&str>) -> Option<Site> {
        let lib = url::Url::parse(web_url).ok()?;
        let origin = lib.origin().ascii_serialization();
        if !origin.starts_with("http") {
            return None;
        }
        let library = decode(lib.path().trim_end_matches('/'));
        let url = match site_url.and_then(|s| url::Url::parse(s).ok()) {
            Some(s) if s.origin() == lib.origin() => s.as_str().trim_end_matches('/').to_string(),
            _ => {
                let mut u = lib.clone();
                u.path_segments_mut().ok()?.pop_if_empty().pop();
                u.set_query(None);
                u.as_str().trim_end_matches('/').to_string()
            }
        };
        Some(Site { url, scope: format!("{origin}/.default"), library })
    }

    /// The last segment of the library's path ("Documents"): `DirName` is compared from it on.
    fn library_name(&self) -> &str {
        self.library.rsplit('/').next().unwrap_or_default()
    }

    /// Where an item deleted from `folder` (below the drive's root, "Fotos/2024") is said to be.
    fn key(&self, folder: &str) -> String {
        let f = folder.trim_matches('/');
        if f.is_empty() { self.library_name().to_string() } else { format!("{}/{f}", self.library_name()) }
    }

    /// Where an item deleted from `folder` is, from the host on ("personal/ana/Documents/Fotos").
    fn full(&self, folder: &str) -> String {
        let f = folder.trim_matches('/');
        let lib = self.library.trim_matches('/');
        if f.is_empty() { lib.to_string() } else { format!("{lib}/{f}") }
    }

    /// `dir` (a `DirName`) as the panes show it: below Meus arquivos when it's in the library.
    fn origin(&self, dir: &str) -> String {
        let d = dir.trim_matches('/');
        let lib = self.library.trim_matches('/');
        let files = Area::Files.label();
        // By `get`: a byte count can fall inside a character of another folder's name.
        let rest = if d.eq_ignore_ascii_case(lib) {
            Some("")
        } else if d.get(..lib.len()).is_some_and(|head| head.eq_ignore_ascii_case(lib)) && d.as_bytes().get(lib.len()) == Some(&b'/') {
            d.get(lib.len() + 1..)
        } else {
            None
        };
        match rest {
            Some("") => files.to_string(),
            Some(r) => format!("{files}/{r}"),
            None => d.to_string(),
        }
    }
}

fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%'
            && i + 2 < b.len()
            && let Ok(x) = u8::from_str_radix(&s[i + 1..i + 3], 16)
        {
            out.push(x);
            i += 3;
            continue;
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether a bin item deleted from `dir` is the one deleted from where `key` says (the library's
/// name and the folder below it).
fn same_place(dir: &str, key: &str) -> bool {
    let d = dir.trim_matches('/').to_lowercase();
    let k = key.trim_matches('/').to_lowercase();
    d == k || d.ends_with(&format!("/{k}"))
}

/// A recycle bin item's id, checked before it goes into an address.
fn guid(id: &str) -> Res<&str> {
    if !id.is_empty() && id.len() <= 40 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
        Ok(id)
    } else {
        Err(Fail::new(t("Resposta inesperada do SharePoint.", "Unexpected answer from SharePoint.")))
    }
}

/// SharePoint's numbers come as JSON numbers or, for 64-bit ones, as strings.
fn number(v: Option<&Value>) -> u64 {
    match v {
        Some(Value::Number(n)) => n.as_u64().unwrap_or(0),
        Some(Value::String(s)) => s.parse().unwrap_or(0),
        _ => 0,
    }
}

fn time(v: Option<&Value>) -> i64 {
    v.and_then(Value::as_str).and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok()).map(|d| d.timestamp()).unwrap_or(0)
}

/// The sentence for an error SharePoint answered with (`odata.error` with
/// `odata=nometadata`, `error` otherwise; the message a string or `{"value": …}`).
pub(crate) fn sp_failure(status: u16, body: &str, what: &str) -> Fail {
    let v: Value = serde_json::from_str(body).unwrap_or_default();
    let err = v.get("odata.error").or_else(|| v.get("error")).cloned().unwrap_or_default();
    let code = err.get("code").and_then(Value::as_str).unwrap_or_default().to_string();
    let message = err.pointer("/message/value").or_else(|| err.get("message")).and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| body.chars().take(300).collect());
    let text = match status {
        404 => tr!("{what}: o item não está mais na lixeira.", "{what}: the item is no longer in the recycle bin."),
        401 | 403 => tr!("{what}: o SharePoint negou o acesso ({message}).", "{what}: SharePoint denied access ({message})."),
        _ => tr!("{what}: o SharePoint respondeu {status}: {message}", "{what}: SharePoint answered {status}: {message}"),
    };
    Fail { status, code, text }
}

/// Gives the bin items unique names, as the panes need: the oldest keeps its own, the others
/// get " (2)", " (3)"… before the extension.
fn name_twins(items: &mut [BinItem]) {
    items.sort_by(|a, b| a.deleted.cmp(&b.deleted).then_with(|| a.id.cmp(&b.id)));
    let mut seen: HashMap<String, usize> = HashMap::new();
    let mut taken: std::collections::HashSet<String> = items.iter().map(|b| b.leaf.to_lowercase()).collect();
    for b in items.iter_mut() {
        let n = seen.entry(b.leaf.to_lowercase()).or_insert(0);
        *n += 1;
        if *n == 1 {
            b.name = b.leaf.clone();
            continue;
        }
        let (stem, ext) = match b.leaf.rfind('.') {
            Some(i) if i > 0 && !b.is_dir => (&b.leaf[..i], &b.leaf[i..]),
            _ => (b.leaf.as_str(), ""),
        };
        let mut k = *n;
        loop {
            let candidate = format!("{stem} ({k}){ext}");
            if taken.insert(candidate.to_lowercase()) {
                b.name = candidate;
                break;
            }
            k += 1;
        }
    }
    items.sort_by(|a, b| b.deleted.cmp(&a.deleted).then_with(|| a.name.cmp(&b.name)));
}

/// Runs `work` on each of `items`, a few at once: each one's result, in order.
fn each<T: Sync, R: Send>(items: &[T], work: impl Fn(&T) -> R + Sync) -> Vec<R> {
    let next = AtomicUsize::new(0);
    let out: Mutex<Vec<Option<R>>> = Mutex::new((0..items.len()).map(|_| None).collect());
    std::thread::scope(|sc| {
        for _ in 0..AT_ONCE.min(items.len()) {
            sc.spawn(|| {
                loop {
                    let i = next.fetch_add(1, Ordering::SeqCst);
                    let Some(it) = items.get(i) else { break };
                    let r = work(it);
                    out.lock().unwrap_or_else(|e| e.into_inner())[i] = Some(r);
                }
            });
        }
    });
    out.into_inner().unwrap_or_else(|e| e.into_inner()).into_iter().map(|r| r.expect("every item ran")).collect()
}

/// An item Graph deleted on a work account, kept for Desfazer.
#[derive(Clone, Debug)]
struct Gone {
    leaf: String,
    /// The folder it was in, below the drive's root.
    folder: String,
    /// Its recycle bin item, once found.
    bin: Option<String>,
}

/// The recycle bin's side of an account.
#[derive(Default)]
pub(crate) struct Side {
    site: Mutex<Option<Site>>,
    /// The collection the bin is read from: `site/RecycleBin`, or `web/RecycleBin` where the
    /// site collection's is refused.
    web_only: Mutex<bool>,
    digest: Mutex<Option<String>>,
    /// What the Lixeira listed last, and whether it can be reached.
    shown: Mutex<Listed>,
    /// Items Graph deleted, by Graph id.
    gone: Mutex<HashMap<String, Gone>>,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

impl Account {
    /// The rest of `path` below the Lixeira, when it's in this account's.
    pub(crate) fn in_bin(&self, path: &str) -> Option<Vec<String>> {
        let segs: Vec<String> = path.split('/').filter(|s| !s.is_empty()).map(str::to_string).collect();
        (self.area(segs.first()?)? == Area::Bin).then(|| segs[1..].to_vec())
    }

    fn business(&self) -> Res<()> {
        if self.meta.kind == Kind::Business {
            Ok(())
        } else {
            Err(Fail::new(t("Contas pessoais não mostram a lixeira: o Microsoft Graph não a lista.", "Personal accounts don't show the recycle bin: Microsoft Graph doesn't list it.")))
        }
    }

    /// The drive's site, asked of Graph once.
    pub(crate) fn site(&self) -> Res<Site> {
        if let Some(s) = lock(&self.bin.site).clone() {
            return Ok(s);
        }
        self.business()?;
        let v = self.graph.get(&format!("/drives/{}/root?$select=id,webUrl,sharepointIds", segment(&self.meta.id)), t("Abrir a lixeira", "Open the recycle bin"))?;
        let web = v.get("webUrl").and_then(Value::as_str).unwrap_or_default();
        let site = Site::of(web, v.pointer("/sharepointIds/siteUrl").and_then(Value::as_str)).ok_or_else(|| Fail::new(t("O OneDrive não deu o endereço do site da conta.", "OneDrive gave no address for the account's site.")))?;
        *lock(&self.bin.site) = Some(site.clone());
        Ok(site)
    }

    /// One request to SharePoint's REST API (`rel` below `{site}/_api/`, or a whole address
    /// SharePoint gave): its answer as JSON.
    fn sp(&self, method: &str, rel: &str, what: &str) -> Res<Value> {
        let site = self.site()?;
        let url = if rel.starts_with("http") { rel.to_string() } else { format!("{}/_api/{rel}", site.url) };
        // SharePoint's own links (the next page) only on the site's host: the token goes nowhere else.
        let same_host = match (url::Url::parse(&url), url::Url::parse(&site.url)) {
            (Ok(a), Ok(b)) => a.origin() == b.origin(),
            _ => false,
        };
        if !same_host {
            return Err(Fail::new(t("Resposta inesperada do SharePoint.", "Unexpected answer from SharePoint.")));
        }
        for asked_digest in [false, true] {
            let digest = lock(&self.bin.digest).clone();
            let mut headers = vec![("Accept", "application/json;odata=nometadata")];
            if method == "POST"
                && let Some(d) = digest.as_deref()
            {
                headers.push(("X-RequestDigest", d));
            }
            let reply = match self.graph.send_as(Some(&site.scope), method, &url, Body::None, &headers) {
                Ok(r) => r,
                Err(f) if f.code == NEEDS_CONSENT => {
                    lock(&self.bin.shown).0 = Access::NeedsConsent(f.text.clone());
                    return Err(Fail { text: consent_help(), ..f });
                }
                Err(f) => return Err(f),
            };
            lock(&self.bin.shown).0 = Access::Ready;
            let wants_digest = reply.status == 403 && method == "POST" && (reply.body.contains("security validation") || reply.body.contains("RequestDigest"));
            if wants_digest && !asked_digest {
                let info = self.graph.send_as(Some(&site.scope), "POST", &format!("{}/_api/contextinfo", site.url), Body::None, &[("Accept", "application/json;odata=nometadata")])?;
                let v: Value = serde_json::from_str(&info.body).unwrap_or_default();
                let d = v.get("FormDigestValue").or_else(|| v.pointer("/d/GetContextWebInformation/FormDigestValue")).and_then(Value::as_str);
                *lock(&self.bin.digest) = d.map(str::to_string);
                log(&format!("OneDrive {}: o SharePoint pediu X-RequestDigest", self.meta.id));
                continue;
            }
            if !reply.ok() {
                return Err(sp_failure(reply.status, &reply.body, what));
            }
            if reply.body.trim().is_empty() {
                return Ok(Value::Null);
            }
            return serde_json::from_str(&reply.body).map_err(|e| Fail::new(tr!("{what}: resposta inesperada: {e}", "{what}: unexpected answer: {e}")));
        }
        Err(Fail::new(tr!("{what}: o SharePoint recusou a validação do pedido.", "{what}: SharePoint refused the request's validation.")))
    }

    fn collection(&self) -> &'static str {
        if *lock(&self.bin.web_only) { "web/RecycleBin" } else { "site/RecycleBin" }
    }

    /// The bin's items as SharePoint has them now, `since` (seconds since 1970) on if given.
    fn read_bin(&self, since: Option<i64>) -> Res<Vec<BinItem>> {
        let site = self.site()?;
        let what = t("Ler a lixeira", "Read the recycle bin");
        let select = "$select=Id,LeafName,DirName,DeletedDate,ItemState,ItemType,Size";
        let filter = since.and_then(|s| chrono::DateTime::from_timestamp(s, 0)).map(|d| format!("&$filter=DeletedDate%20ge%20datetime%27{}%27", d.format("%Y-%m-%dT%H:%M:%SZ"))).unwrap_or_default();
        let mut url = format!("{}?{select}{filter}", self.collection());
        let mut raw: Vec<Value> = Vec::new();
        let mut filtered = !filter.is_empty();
        let mut retried_web = false;
        loop {
            match self.sp("GET", &url, what) {
                Ok(v) => {
                    raw.extend(v.get("value").and_then(Value::as_array).into_iter().flatten().cloned());
                    match v.get("odata.nextLink").or_else(|| v.get("@odata.nextLink")).and_then(Value::as_str) {
                        Some(next) => url = next.to_string(),
                        None => break,
                    }
                }
                // The site collection's bin refused: the site's own (the first stage).
                Err(f) if matches!(f.status, 401 | 403) && !retried_web && !*lock(&self.bin.web_only) => {
                    log(&format!("OneDrive {}: lixeira do conjunto de sites recusada ({}); lendo a do site", self.meta.id, f.status));
                    *lock(&self.bin.web_only) = true;
                    retried_web = true;
                    raw.clear();
                    url = format!("{}?{select}{filter}", self.collection());
                }
                // Refused on both: the token holds a SharePoint permission that isn't enough
                // (another than AllSites.Write): what to grant, with SharePoint's words.
                Err(f) if matches!(f.status, 401 | 403) => {
                    lock(&self.bin.shown).0 = Access::NeedsConsent(f.text.clone());
                    return Err(Fail { text: consent_help(), ..f });
                }
                Err(f) if f.code == NEEDS_CONSENT => return Err(f),
                // A filter it can't take (whatever it answers): the whole bin, filtered here.
                Err(_) if filtered => {
                    filtered = false;
                    raw.clear();
                    url = format!("{}?{select}", self.collection());
                }
                Err(f) => return Err(f),
            }
        }
        let mut items: Vec<BinItem> = raw
            .iter()
            .filter_map(|v| {
                let id = v.get("Id").and_then(Value::as_str)?.to_string();
                let leaf = v.get("LeafName").and_then(Value::as_str)?.to_string();
                let dir = v.get("DirName").and_then(Value::as_str).unwrap_or_default().to_string();
                let kind = number(v.get("ItemType"));
                Some(BinItem {
                    origin: site.origin(&dir),
                    id,
                    name: leaf.clone(),
                    leaf,
                    dir,
                    deleted: time(v.get("DeletedDate")),
                    size: number(v.get("Size")),
                    // ItemType: 5 a folder, 6 a folder with lists.
                    is_dir: matches!(kind, 5 | 6),
                    second_stage: number(v.get("ItemState")) == 2,
                })
            })
            .filter(|b| since.is_none_or(|s| b.deleted >= s))
            .collect();
        name_twins(&mut items);
        Ok(items)
    }

    /// Reads the bin and keeps what it shows.
    pub fn bin_items(&self) -> Result<Vec<BinItem>> {
        let items = self.read_bin(None)?;
        lock(&self.bin.shown).1 = Arc::new(items.clone());
        Ok(items)
    }

    pub(crate) fn bin_list_at(&self, rest: &[String]) -> Result<Vec<Item>> {
        if !rest.is_empty() {
            return Err(t("Uma pasta na lixeira não abre: restaure-a para ver o que tem dentro.", "A folder in the recycle bin doesn't open: restore it to see what it holds.").into());
        }
        Ok(self.bin_items()?.into_iter().map(|b| Item { name: b.name, is_dir: b.is_dir, size: (!b.is_dir).then_some(b.size), modified: (b.deleted > 0).then_some(b.deleted), id: Some(b.id) }).collect())
    }

    pub(crate) fn bin_stat(&self, rest: &[String]) -> Result<Option<Item>> {
        Ok(match rest {
            [] => Some(Item { name: Area::Bin.label().to_string(), is_dir: true, size: None, modified: None, id: None }),
            [name] => {
                let found = |items: &[BinItem]| items.iter().find(|b| b.name == *name).cloned();
                let b = match found(&self.bin_shown()) {
                    Some(b) => Some(b),
                    None => found(&self.bin_items()?),
                };
                b.map(|b| Item { name: b.name, is_dir: b.is_dir, size: (!b.is_dir).then_some(b.size), modified: (b.deleted > 0).then_some(b.deleted), id: Some(b.id) })
            }
            _ => None,
        })
    }

    /// The listed items named `names` (as shown), all found before anything changes.
    fn bin_named(&self, names: &[String]) -> Res<Vec<BinItem>> {
        let shown = self.bin_shown();
        names.iter().map(|n| shown.iter().find(|b| b.name == *n).cloned().ok_or_else(|| Fail::new(tr!("“{n}” não está mais na lixeira: atualize a pasta.", "“{n}” is no longer in the recycle bin: refresh the folder.")))).collect()
    }

    fn bin_each(&self, items: &[BinItem], verb: &str, what: &'static str) -> (Vec<String>, Option<String>) {
        let coll = self.collection();
        let results = each(items, |b| guid(&b.id).and_then(|id| self.sp("POST", &format!("{coll}('{id}')/{verb}()"), &b.leaf)).map(|_| ()));
        let mut done = Vec::new();
        let mut first = None;
        for (b, r) in items.iter().zip(results) {
            match r {
                Ok(()) => done.push(b.name.clone()),
                Err(f) if first.is_none() => first = Some(if f.text.is_empty() { what.to_string() } else { f.text }),
                Err(_) => {}
            }
        }
        {
            let mut shown = lock(&self.bin.shown);
            shown.1 = Arc::new(shown.1.iter().filter(|b| !done.contains(&b.name)).cloned().collect());
        }
        (done, first)
    }

    /// Restaurar: the items (by the names the Lixeira shows) go back where they were; the
    /// folders they land in are brought up to date. The names restored, and the first error.
    pub fn bin_restore(&self, names: &[String]) -> (Vec<String>, Option<String>) {
        let items = match self.bin_named(names) {
            Ok(i) => i,
            Err(e) => return (Vec::new(), Some(e.text)),
        };
        let out = self.bin_each(&items, "restore", t("Restaurar", "Restore"));
        if !out.0.is_empty() {
            let _ = self.catch_up();
        }
        out
    }

    /// Excluir permanentemente: the items (by the names shown) are deleted for good.
    pub fn bin_purge(&self, names: &[String]) -> (Vec<String>, Option<String>) {
        match self.bin_named(names) {
            Ok(items) => self.bin_each(&items, "deleteObject", t("Excluir", "Delete")),
            Err(e) => (Vec::new(), Some(e.text)),
        }
    }

    /// Esvaziar lixeira: every item the bin holds as read now, deleted for good, one by one
    /// (what arrives meanwhile stays). How many went, or the first error.
    pub fn bin_empty(&self) -> Result<usize> {
        let items = self.bin_items()?;
        let (done, err) = self.bin_each(&items, "deleteObject", t("Esvaziar", "Empty"));
        match err {
            Some(e) if done.is_empty() => Err(e),
            Some(e) => Err(tr!("{e} (já excluídos para sempre: {})", "{e} (already deleted for good: {})", done.len())),
            None => Ok(done.len()),
        }
    }

    /// A sign-in that grants the app SharePoint for this account (the Lixeira's Autorizar).
    pub fn bin_grant(&self) -> Result<PendingLogin> {
        let site = self.site()?;
        Ok(crate::oauth::grant(self.graph.client(), &self.graph.ends, &site.scope, &self.meta.email)?)
    }

    /// Ends [`Account::bin_grant`]'s sign-in: SharePoint's token taken in, the bin read.
    pub fn bin_granted(&self, login: &PendingLogin) -> Result<()> {
        let site = self.site()?;
        let answer = login.finish().map_err(|f| if f.code == NEEDS_CONSENT { Fail { text: format!("{}\n\n{}", consent_help(), f.text), ..f } } else { f })?;
        self.graph.adopt(&site.scope, &answer);
        lock(&self.bin.shown).0 = Access::Unknown;
        self.bin_items().map(|_| ())
    }

    /// What the Lixeira listed last.
    pub fn bin_shown(&self) -> Arc<Vec<BinItem>> {
        lock(&self.bin.shown).1.clone()
    }

    /// Whether the Lixeira can be reached, as last seen.
    pub fn bin_access(&self) -> Access {
        lock(&self.bin.shown).0.clone()
    }

    // -----------------------------------------------------------------------------------------
    // Desfazer on work accounts

    /// After Graph deleted `items` (Graph id, name, folder below the root) at `since`: their
    /// recycle bin items, found by name, folder and time, kept for Desfazer. Nothing is found
    /// (and nothing fails) while SharePoint isn't granted.
    pub(crate) fn note_gone(&self, items: &[(String, String, String)], since: i64) {
        if self.meta.kind != Kind::Business || items.is_empty() {
            return;
        }
        {
            let mut gone = lock(&self.bin.gone);
            for (id, leaf, folder) in items {
                gone.insert(id.clone(), Gone { leaf: leaf.clone(), folder: folder.clone(), bin: None });
            }
        }
        let (site, found) = match self.site().and_then(|s| self.read_bin(Some(since - SKEW)).map(|f| (s, f))) {
            Ok(x) => x,
            Err(f) => {
                log(&format!("OneDrive {}: sem a lixeira para desfazer ({})", self.meta.id, f.text));
                return;
            }
        };
        let mut gone = lock(&self.bin.gone);
        // A bin item already matched to another deletion isn't this one.
        let mut taken: Vec<String> = gone.values().filter_map(|g| g.bin.clone()).collect();
        for (id, _, _) in items {
            let Some(g) = gone.get_mut(id) else { continue };
            let key = site.key(&g.folder);
            let full = site.full(&g.folder);
            let named: Vec<&BinItem> = found.iter().filter(|b| !taken.contains(&b.id) && b.leaf.eq_ignore_ascii_case(&g.leaf)).collect();
            // The newest that fits (a twin deleted earlier from the same folder is older): from
            // the library's whole path first, from its name on only when none is.
            let hit = named.iter().filter(|b| b.dir.trim_matches('/').to_lowercase() == full.to_lowercase()).max_by_key(|b| b.deleted).or_else(|| named.iter().filter(|b| same_place(&b.dir, &key)).max_by_key(|b| b.deleted));
            match hit {
                Some(b) => {
                    taken.push(b.id.clone());
                    g.bin = Some(b.id.clone());
                }
                None => log(&format!("OneDrive {}: não achei “{}” na lixeira", self.meta.id, g.leaf)),
            }
        }
    }

    /// Whether Desfazer can bring item `id` back from the recycle bin.
    pub fn can_restore(&self, id: &str) -> bool {
        match self.meta.kind {
            Kind::Personal => true,
            Kind::Business => lock(&self.bin.gone).get(id).is_some_and(|g| g.bin.is_some()),
        }
    }

    /// Desfazer on a work account: the bin items Graph ids `ids` went to, restored.
    pub(crate) fn restore_business(&self, ids: &[String]) -> (Vec<String>, Option<String>) {
        let found: Vec<(String, Gone)> = {
            let gone = lock(&self.bin.gone);
            ids.iter().filter_map(|id| gone.get(id).map(|g| (id.clone(), g.clone()))).collect()
        };
        let mut first = None;
        let ready: Vec<(String, Gone)> = found.into_iter().filter(|(_, g)| g.bin.is_some()).collect();
        if ready.len() < ids.len() {
            first = Some(t("O item não pode ser restaurado por aqui: autorize a Lixeira da conta ou restaure pelo site.", "The item can't be restored from here: authorize the account's recycle bin or restore it on the website.").to_string());
        }
        let coll = self.collection();
        let results = each(&ready, |(_, g)| guid(g.bin.as_deref().unwrap_or_default()).and_then(|b| self.sp("POST", &format!("{coll}('{b}')/restore()"), &g.leaf)).map(|_| ()));
        let mut done = Vec::new();
        for ((id, _), r) in ready.iter().zip(results) {
            match r {
                Ok(()) => done.push(id.clone()),
                Err(f) if first.is_none() => first = Some(f.text),
                Err(_) => {}
            }
        }
        {
            let mut gone = lock(&self.bin.gone);
            for id in &done {
                if let Some(g) = gone.get_mut(id) {
                    g.bin = None;
                }
            }
        }
        if !done.is_empty() {
            let _ = self.catch_up();
        }
        (done, first)
    }

    /// Where item `id` was when the app deleted it (Refazer sends it there again).
    pub(crate) fn gone_place(&self, id: &str) -> Option<(String, String)> {
        lock(&self.bin.gone).get(id).map(|g| (g.leaf.clone(), g.folder.clone()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn site() -> Site {
        Site::of("https://contoso-my.sharepoint.com/personal/ana_contoso_com/Documents", Some("https://contoso-my.sharepoint.com/personal/ana_contoso_com")).unwrap()
    }

    #[test]
    fn the_site_comes_from_the_library_and_only_its_host_gets_the_token() {
        let s = site();
        assert_eq!(s.url, "https://contoso-my.sharepoint.com/personal/ana_contoso_com");
        assert_eq!(s.scope, "https://contoso-my.sharepoint.com/.default");
        assert_eq!(s.library, "/personal/ana_contoso_com/Documents");
        // No site said, or one on another host: the library's parent.
        let s = Site::of("https://contoso-my.sharepoint.com/personal/ana_contoso_com/Documents", Some("https://evil.example.com/x")).unwrap();
        assert_eq!(s.url, "https://contoso-my.sharepoint.com/personal/ana_contoso_com");
        let s = Site::of("https://contoso-my.sharepoint.com/personal/jo%C3%A3o_contoso_com/Documentos%20Compartilhados", None).unwrap();
        assert_eq!(s.library, "/personal/joão_contoso_com/Documentos Compartilhados");
        assert!(Site::of("not a url", None).is_none());
    }

    #[test]
    fn deleted_items_are_matched_by_their_folder_from_the_library_on() {
        let s = site();
        assert_eq!(s.key(""), "Documents");
        assert_eq!(s.key("/Fotos/2024/"), "Documents/Fotos/2024");
        assert!(same_place("personal/ana_contoso_com/Documents/Fotos/2024", &s.key("Fotos/2024")));
        assert!(same_place("/personal/ana_contoso_com/documents", &s.key("")));
        assert!(!same_place("personal/ana_contoso_com/Documents/Fotos/2024", &s.key("2024")));
        assert!(!same_place("personal/ana_contoso_com/Documents/Outras Fotos", &s.key("Fotos")));
        assert_eq!(s.origin("personal/ana_contoso_com/Documents/Fotos"), "Meus arquivos/Fotos");
        assert_eq!(s.origin("/personal/ana_contoso_com/Documents"), "Meus arquivos");
        assert_eq!(s.origin("sites/outro/Lists/Tarefas"), "sites/outro/Lists/Tarefas");
        // A character across the library's length doesn't split.
        assert_eq!(s.origin("personal/ana_contoso_com/Documentosé/x"), "personal/ana_contoso_com/Documentosé/x");
        assert_eq!(s.origin("personal/ana_contoso_com/Documenté/x"), "personal/ana_contoso_com/Documenté/x");
    }

    #[test]
    fn twins_in_the_bin_get_names_of_their_own() {
        let b = |id: &str, leaf: &str, deleted: i64, is_dir: bool| BinItem { id: id.into(), name: String::new(), leaf: leaf.into(), dir: String::new(), origin: String::new(), deleted, size: 0, is_dir, second_stage: false };
        let mut items = vec![b("3", "nota.txt", 30, false), b("1", "nota.txt", 10, false), b("2", "Nota.txt", 20, false), b("4", "nota (2).txt", 5, false), b("5", "Pasta.v2", 1, true), b("6", "Pasta.v2", 2, true)];
        name_twins(&mut items);
        let by_id = |id: &str| items.iter().find(|x| x.id == id).unwrap().name.clone();
        assert_eq!(by_id("1"), "nota.txt");
        assert_eq!(by_id("4"), "nota (2).txt");
        assert_eq!(by_id("2"), "Nota (3).txt");
        assert_eq!(by_id("3"), "nota (4).txt");
        assert_eq!(by_id("6"), "Pasta.v2 (2)");
        // Newest first.
        assert_eq!(items[0].id, "3");
    }

    #[test]
    fn sharepoint_answers_read_as_sentences() {
        let f = sp_failure(404, r#"{"odata.error":{"code":"-2147024809, System.ArgumentException","message":{"lang":"en-US","value":"Item does not exist."}}}"#, "Restaurar");
        assert!(f.text.contains("não está mais na lixeira") && f.code.contains("ArgumentException"));
        let f = sp_failure(403, r#"{"error":{"code":"x","message":"Access denied."}}"#, "Ler");
        assert!(f.text.contains("Access denied."));
        assert_eq!(number(Some(&Value::String("1234".into()))), 1234);
        assert_eq!(time(Some(&Value::String("2024-05-06T07:08:09Z".into()))), 1714979289);
        assert!(guid("9a1b2c3d-0000-4e5f-8a9b-0123456789ab").is_ok() && guid("x')/deleteAll(").is_err());
        assert_eq!(decode("a%20b%C3%A3%"), "a bã%");
    }
}
