//! Many items in a few requests: moves and copies within the account (`move_batch_v2`,
//! `copy_batch_v2`, a job asked about at `…/check_v2` until it ends) and small files sent
//! together (`upload_session/start_batch`, one `upload_session/append_batch` with all their
//! bytes, one `upload_session/finish_batch_v2` that commits them all at once).
//!
//! Dropbox takes up to 1,000 entries per relocation and finish batch, and 500 per append batch
//! (150 MiB at most); a batch commits under one lock on the account, where files sent one by one
//! each take it and meet `too_many_write_operations` when several go at once. Entries Dropbox
//! was too busy for (or that failed on its side, `internal_error`) go again in another batch,
//! a few rounds, then one by one, which waits as long as Dropbox asks. Every other failure is
//! that entry's own (a name taken, a folder into itself), as in the one-item calls: nothing is
//! renamed on the way (`autorename` stays off).
//!
//! A batch whose answer was lost (the job's launch, its check, the commit) isn't sent again
//! blindly: the folders on both sides are read once, entries found done are taken as done, and
//! only the others go again.

use std::collections::HashMap;
use std::path::PathBuf;

use openexplorer_i18n::{t, tr};
use serde_json::{Value, json};

use crate::account::{Account, failure_summary};
use crate::api::{Body, Fail};
use crate::content::Hasher;
use crate::meta::{Change, Entry, api_path, entry, key, last, parent};
use crate::transfer::{BLOCK, correct_offset};
use crate::{CANCELLED, Result};

/// Entries per relocation batch: the most Dropbox takes.
const RELOCATE_MAX: usize = 1000;
/// Files per upload batch. Dropbox takes 500 per append and 1,000 per finish; the finish answers
/// only once all are committed (it's given [`crate::api::commit_timeout`] for that), and a
/// smaller batch keeps what a lost answer leaves to check small.
pub const UPLOAD_FILES: usize = 250;
/// Bytes per upload batch: all held in memory, to be sent again when Dropbox asks to wait.
const UPLOAD_BYTES: usize = 32 << 20;
/// Files up to this size go in an upload batch; a bigger one goes alone, in its own session.
pub const BATCH_FILE_MAX: u64 = BLOCK as u64;
/// Rounds of a batch for the entries Dropbox was too busy for, before going one by one.
const ROUNDS: usize = 3;

/// How one entry of a batch ended.
enum Outcome {
    Done(Entry),
    /// Dropbox was too busy, or failed on its side: worth another try.
    Busy,
    /// The entry's own failure: an `error_summary`-like text.
    Failed(String),
}

/// A batch entry's failure object: worth another try, or its summary.
fn failed(f: &Value) -> Outcome {
    let summary = failure_summary(f);
    let busy = summary.split(['/', '.']).any(|w| w == "too_many_write_operations" || w == "internal_error");
    if busy { Outcome::Busy } else { Outcome::Failed(summary) }
}

/// The content hash of `data`.
fn hash_of(data: &[u8]) -> String {
    let mut h = Hasher::default();
    h.update(data);
    h.finish()
}

/// An item's failure in the user's words.
fn named(path: &str, summary: &str) -> String {
    format!("“{}”: {}", last(&api_path(path)), crate::api::route_text(summary))
}

impl Account {
    /// Moves (renames) each `(from, to)` within the account, in batches: each one's result, in
    /// order. Fails where something is at `to`, as [`Account::rename`]; a change of case alone,
    /// which a batch can't make, goes through [`Account::rename`].
    pub fn move_many(&self, pairs: &[(String, String)]) -> Vec<Result<Entry>> {
        self.relocate_many(pairs, true)
    }

    /// Copies each `(from, to)` within the account, on Dropbox's side, in batches: each one's
    /// result, in order.
    pub fn copy_many(&self, pairs: &[(String, String)]) -> Vec<Result<Entry>> {
        self.relocate_many(pairs, false)
    }

    fn relocate_many(&self, pairs: &[(String, String)], moving: bool) -> Vec<Result<Entry>> {
        let mut out: Vec<Option<Result<Entry>>> = vec![None; pairs.len()];
        let mut todo = Vec::new();
        for (i, (from, to)) in pairs.iter().enumerate() {
            if moving && key(from) == key(to) {
                out[i] = Some(self.rename(from, to));
            } else {
                todo.push(i);
            }
        }
        for _ in 0..ROUNDS {
            if todo.is_empty() {
                break;
            }
            let mut again = Vec::new();
            for chunk in todo.chunks(RELOCATE_MAX) {
                let entries: Vec<(&str, &str)> = chunk.iter().map(|&i| (pairs[i].0.as_str(), pairs[i].1.as_str())).collect();
                match self.relocate_batch(&entries, moving) {
                    Err(e) => chunk.iter().for_each(|&i| out[i] = Some(Err(e.clone()))),
                    Ok(results) => {
                        let mut changes = Vec::new();
                        for (&i, r) in chunk.iter().zip(results) {
                            match r {
                                Outcome::Done(e) => {
                                    if moving {
                                        changes.push(Change::Gone(key(&pairs[i].0)));
                                    }
                                    changes.push(Change::Is(e.clone()));
                                    out[i] = Some(Ok(e));
                                }
                                Outcome::Busy => again.push(i),
                                Outcome::Failed(summary) => out[i] = Some(Err(named(&pairs[i].0, &summary))),
                            }
                        }
                        self.note(&changes);
                    }
                }
            }
            todo = again;
        }
        // Still too busy: one by one, each waiting as long as Dropbox asks.
        for i in todo {
            let (from, to) = &pairs[i];
            out[i] = Some(if moving { self.rename(from, to) } else { self.copy(from, to) });
        }
        out.into_iter().map(|r| r.expect("every entry ends")).collect()
    }

    /// One relocation batch job, to its end: each entry's outcome, in order.
    fn relocate_batch(&self, entries: &[(&str, &str)], moving: bool) -> Result<Vec<Outcome>> {
        let (route, check) = if moving { ("files/move_batch_v2", "files/move_batch/check_v2") } else { ("files/copy_batch_v2", "files/copy_batch/check_v2") };
        let list: Vec<Value> = entries.iter().map(|(f, to)| json!({"from_path": api_path(f), "to_path": api_path(to)})).collect();
        let mut arg = json!({"entries": list, "autorename": false});
        if moving {
            arg["allow_ownership_transfer"] = json!(false);
        }
        let answer = {
            let _gate = self.batch_gate.lock().unwrap_or_else(|e| e.into_inner());
            self.api.rpc(route, &arg).and_then(|first| self.wait_job(check, first))
        };
        let done = match answer {
            Ok(v) => v,
            // Whether the job ran is unknown: each entry as Dropbox has it now (the folders on
            // both sides read once); what isn't done goes again.
            Err(Fail::Lost(e)) => {
                crate::api::log(&format!("Dropbox {route}: a resposta não veio ({e}); conferindo"));
                std::thread::sleep(crate::account::SETTLE);
                let paths: Vec<&str> = entries.iter().flat_map(|(f, t)| [*f, *t]).collect();
                let seen = self.look(&paths).map_err(String::from)?;
                return Ok(entries.iter().map(|(f, t)| Account::relocated(&seen, f, t, moving).map_or(Outcome::Busy, Outcome::Done)).collect());
            }
            Err(f) => return Err(f.text()),
        };
        let results: Vec<Value> = done.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut out: Vec<Outcome> = results
            .iter()
            .map(|r| match r.get(".tag").and_then(Value::as_str) {
                // A `Metadata` member is nested under the tag; read the other forms too.
                Some("success") => match r.get("success").or(r.get("metadata")).and_then(entry).or_else(|| entry(r)) {
                    Some(e) => Outcome::Done(e),
                    None => Outcome::Failed(String::new()),
                },
                _ => r.get("failure").map(failed).unwrap_or(Outcome::Busy),
            })
            .collect();
        // An answer short of entries: those are tried again.
        out.resize_with(entries.len(), || Outcome::Busy);
        Ok(out)
    }

    // -----------------------------------------------------------------------------------------
    // Many small files

    /// Sends each local file `(to, local)` to `to` (replacing what's there): each one's result,
    /// in order. Files up to [`BATCH_FILE_MAX`] go together, a few requests for up to
    /// [`UPLOAD_FILES`] of them; bigger ones each alone, as [`Account::upload`]. A file whose
    /// same bytes are already at `to` isn't sent: the folders it goes to are read once (their
    /// kept listings) to tell. `progress(i, bytes)` hears file `i`'s bytes so far, never fewer
    /// than it heard before; `false` stops that file with [`CANCELLED`], and the others in its
    /// batch go on without it.
    pub fn upload_many(&self, files: &[(String, PathBuf)], progress: &mut dyn FnMut(usize, u64) -> bool) -> Vec<Result<Entry>> {
        let mut out: Vec<Option<Result<Entry>>> = vec![None; files.len()];
        let mut told = vec![0u64; files.len()];
        let mut tell = |i: usize, n: u64| {
            told[i] = told[i].max(n);
            progress(i, told[i])
        };
        // Sizes, and what's there already, folder by folder.
        let mut there: HashMap<String, Option<Vec<Entry>>> = HashMap::new();
        let mut small = Vec::new();
        let mut big = Vec::new();
        for (i, (to, local)) in files.iter().enumerate() {
            let len = match std::fs::metadata(local) {
                Ok(m) => m.len(),
                Err(e) => {
                    out[i] = Some(Err(tr!("Não consegui ler {}: {e}", "Couldn't read {}: {e}", local.display())));
                    continue;
                }
            };
            let k = key(to);
            let listing = there.entry(parent(&k).to_string()).or_insert_with(|| self.list(parent(&api_path(to))).ok());
            let same = listing.as_ref().and_then(|l| l.iter().find(|e| e.lower == k)).filter(|e| !e.dir && e.size == len && !e.hash.is_empty()).cloned();
            if let Some(e) = same
                && crate::content::of_file(local, &|| false).is_ok_and(|h| h == e.hash)
            {
                out[i] = Some(if tell(i, len) { Ok(e) } else { Err(CANCELLED.into()) });
                continue;
            }
            if len <= BATCH_FILE_MAX { small.push((i, len)) } else { big.push((i, len)) }
        }
        // The small ones in batches, by count and by bytes.
        let mut group: Vec<usize> = Vec::new();
        let mut bytes = 0usize;
        for (n, &(i, len)) in small.iter().enumerate() {
            group.push(i);
            bytes += len as usize;
            let next = small.get(n + 1).map_or(0, |&(_, l)| l as usize);
            if group.len() == UPLOAD_FILES || bytes + next > UPLOAD_BYTES || n + 1 == small.len() {
                self.upload_group(files, std::mem::take(&mut group), &mut tell, &mut out);
                bytes = 0;
            }
        }
        for (i, len) in big {
            if !tell(i, 0) {
                out[i] = Some(Err(CANCELLED.into()));
                continue;
            }
            out[i] = Some(match std::fs::File::open(&files[i].1) {
                Ok(mut f) => self.upload(&files[i].0, &mut f, len, &mut |n| tell(i, n)),
                Err(e) => Err(tr!("Não consegui abrir {}: {e}", "Couldn't open {}: {e}", files[i].1.display())),
            });
        }
        out.into_iter().map(|r| r.expect("every file ends")).collect()
    }

    /// One batch of small files, to its end; a file stopped midway leaves it, and the others go
    /// again without it (and without those stopped meanwhile: their progress is asked first, so
    /// a whole folder cancelled costs no request more).
    fn upload_group(&self, files: &[(String, PathBuf)], group: Vec<usize>, tell: &mut dyn FnMut(usize, u64) -> bool, out: &mut [Option<Result<Entry>>]) {
        let mut data: Vec<(usize, Vec<u8>)> = Vec::new();
        let mut stopped = Vec::new();
        for i in group {
            if !tell(i, 0) {
                stopped.push(i);
                continue;
            }
            // The bytes, read once.
            match std::fs::read(&files[i].1) {
                Ok(d) => data.push((i, d)),
                Err(e) => out[i] = Some(Err(tr!("Não consegui ler {}: {e}", "Couldn't read {}: {e}", files[i].1.display()))),
            }
        }
        loop {
            for &i in &stopped {
                out[i] = Some(Err(CANCELLED.into()));
            }
            data.retain(|(i, _)| !stopped.contains(i));
            if data.is_empty() {
                return;
            }
            match self.send_group(files, &data, tell, out) {
                Ok(()) => return,
                Err(mut more) => {
                    more.extend(data.iter().map(|(i, _)| *i).filter(|i| !more.contains(i) && !tell(*i, 0)).collect::<Vec<_>>());
                    stopped = more;
                }
            }
        }
    }

    /// Sends `data` (file index, bytes) in sessions of their own and commits them; `Err` with
    /// the files whose progress said stop, when one did (nothing was committed then).
    fn send_group(&self, files: &[(String, PathBuf)], data: &[(usize, Vec<u8>)], tell: &mut dyn FnMut(usize, u64) -> bool, out: &mut [Option<Result<Entry>>]) -> std::result::Result<(), Vec<usize>> {
        let all = |out: &mut [Option<Result<Entry>>], e: &str| data.iter().for_each(|(i, _)| out[*i] = Some(Err(e.to_string())));
        // A session for each.
        let ids = match self.api.rpc("files/upload_session/start_batch", &json!({"num_sessions": data.len()})) {
            Ok(v) => v.get("session_ids").and_then(Value::as_array).map(|a| a.iter().filter_map(|s| s.as_str().map(str::to_string)).collect::<Vec<_>>()).unwrap_or_default(),
            Err(f) => {
                all(out, &f.text());
                return Ok(());
            }
        };
        if ids.len() != data.len() {
            all(out, t("O Dropbox não abriu as sessões de envio.", "Dropbox didn't open the upload sessions."));
            return Ok(());
        }
        // All the bytes in one request, each file's closing its session.
        let mut body = Vec::with_capacity(data.iter().map(|(_, d)| d.len()).sum());
        let mut ends = Vec::with_capacity(data.len());
        for (_, d) in data {
            body.extend_from_slice(d);
            ends.push(body.len());
        }
        let entries: Vec<Value> = ids.iter().zip(data).map(|(id, (_, d))| json!({"cursor": {"session_id": id, "offset": 0}, "length": d.len(), "close": true})).collect();
        let mut stopped = Vec::new();
        let sent = {
            let mut whole = 0usize;
            let mut told = |n: usize| {
                // Files wholly sent hear their size (once, a try sent again after a 429 starts
                // from 0); the one under way, how far it got.
                let at = ends.partition_point(|&e| e <= n);
                for (i, d) in &data[whole.min(at)..at] {
                    if !tell(*i, d.len() as u64) {
                        stopped.push(*i);
                        return false;
                    }
                }
                whole = whole.max(at);
                if let Some((i, _)) = data.get(at) {
                    let start = if at == 0 { 0 } else { ends[at - 1] };
                    if !tell(*i, (n - start) as u64) {
                        stopped.push(*i);
                        return false;
                    }
                }
                true
            };
            self.api.upload("files/upload_session/append_batch", &json!({"entries": entries}), &mut Body::Bytes(&body, Some(&mut told)))
        };
        let appended = match sent {
            Ok(v) => v,
            Err(Fail::Other(e)) if e == CANCELLED && !stopped.is_empty() => return Err(stopped),
            Err(f) => {
                all(out, &f.text());
                return Ok(());
            }
        };
        // Each session that took its bytes is committed; one that didn't, its file alone.
        let results = appended.get("entries").and_then(Value::as_array).cloned().unwrap_or_default();
        let mut ready = Vec::new();
        for (n, ((i, d), id)) in data.iter().zip(&ids).enumerate() {
            let took = match results.get(n) {
                Some(r) if r.get(".tag").and_then(Value::as_str) == Some("success") => true,
                // Taken already, on a try whose answer was lost.
                Some(r) => r.get("failure").and_then(correct_offset) == Some(d.len() as u64),
                None => false,
            };
            if took {
                ready.push((*i, id.clone(), d));
            } else {
                out[*i] = Some(self.upload(&files[*i].0, &mut &d[..], d.len() as u64, &mut |k| tell(*i, k)));
            }
        }
        self.finish_group(files, ready, out);
        Ok(())
    }

    /// Commits closed sessions `(file index, session, bytes)` in one `finish_batch_v2`, again
    /// for those Dropbox was too busy for, then one by one.
    fn finish_group(&self, files: &[(String, PathBuf)], mut ready: Vec<(usize, String, &Vec<u8>)>, out: &mut [Option<Result<Entry>>]) {
        let commit = |to: &str| json!({"path": api_path(to), "mode": "overwrite", "autorename": false, "mute": true, "strict_conflict": false});
        let mut changes = Vec::new();
        let mut done = |i: usize, e: Entry, d: &[u8], out: &mut [Option<Result<Entry>>]| {
            let mut h = Hasher::default();
            h.update(d);
            out[i] = Some(if !e.hash.is_empty() && e.hash != h.finish() {
                Err(tr!("O arquivo “{}” chegou diferente ao Dropbox (hash não confere).", "The file “{}” reached Dropbox different (hash mismatch).", e.name))
            } else {
                changes.push(Change::Is(e.clone()));
                Ok(e)
            });
        };
        for _ in 0..ROUNDS {
            if ready.is_empty() {
                break;
            }
            let entries: Vec<Value> = ready.iter().map(|(i, id, d)| json!({"cursor": {"session_id": id, "offset": d.len()}, "commit": commit(&files[*i].0)})).collect();
            let answer = {
                let _gate = self.batch_gate.lock().unwrap_or_else(|e| e.into_inner());
                // It answers once all are committed: given as long as that may take.
                self.api.rpc_waiting("files/upload_session/finish_batch_v2", &json!({"entries": entries}), crate::api::commit_timeout(entries.len()))
            };
            let results = match answer {
                Ok(v) => v.get("entries").and_then(Value::as_array).cloned().unwrap_or_default(),
                // Whether it committed is unknown: the files whose bytes are where they go
                // (their folders read once) are done, the others committed again.
                Err(Fail::Lost(e)) => {
                    crate::api::log(&format!("Dropbox finish_batch_v2: a resposta não veio ({e}); conferindo"));
                    std::thread::sleep(crate::account::SETTLE);
                    let paths: Vec<&str> = ready.iter().map(|(i, ..)| files[*i].0.as_str()).collect();
                    let seen = match self.look(&paths) {
                        Ok(s) => s,
                        Err(f) => {
                            for (i, ..) in &ready {
                                out[*i] = Some(Err(format!("“{}”: {}", last(&api_path(&files[*i].0)), f.text())));
                            }
                            ready.clear();
                            break;
                        }
                    };
                    let mut again = Vec::new();
                    for (i, id, d) in ready {
                        match seen.get(&key(&files[i].0)).filter(|e| !e.dir && e.size == d.len() as u64 && e.hash == hash_of(d)) {
                            Some(e) => done(i, e.clone(), d, out),
                            None => again.push((i, id, d)),
                        }
                    }
                    ready = again;
                    continue;
                }
                Err(f) => {
                    for (i, ..) in &ready {
                        out[*i] = Some(Err(format!("“{}”: {}", last(&api_path(&files[*i].0)), f.text())));
                    }
                    ready.clear();
                    break;
                }
            };
            let mut again = Vec::new();
            for (n, (i, id, d)) in ready.into_iter().enumerate() {
                let r = results.get(n);
                match r.map(|r| (r.get(".tag").and_then(Value::as_str), r)) {
                    // A `FileMetadata` member is inlined beside the entry's tag.
                    Some((Some("success"), r)) => match r.get("success").and_then(entry).or_else(|| entry(r)) {
                        Some(e) => done(i, e, d, out),
                        None => out[i] = Some(Err(t("O Dropbox respondeu sem os dados do arquivo.", "Dropbox answered without the file's details.").into())),
                    },
                    Some((_, r)) => match r.get("failure").map(failed) {
                        Some(Outcome::Failed(summary)) => out[i] = Some(Err(named(&files[i].0, &summary))),
                        _ => again.push((i, id, d)),
                    },
                    None => again.push((i, id, d)),
                }
            }
            ready = again;
        }
        // Still too busy: each session finished alone, waiting as long as Dropbox asks.
        for (i, id, d) in ready {
            match self.finish(&id, d.len() as u64, &commit(&files[i].0), &hash_of(d)) {
                Ok(v) => match entry(&v) {
                    Some(e) => done(i, e, d, out),
                    None => out[i] = Some(Err(t("O Dropbox respondeu sem os dados do arquivo.", "Dropbox answered without the file's details.").into())),
                },
                Err(f) => out[i] = Some(Err(format!("“{}”: {}", last(&api_path(&files[i].0)), f.text()))),
            }
        }
        drop(done);
        self.note(&changes);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake::Fake;

    fn names(a: &Account, dir: &str) -> Vec<String> {
        let mut n: Vec<String> = a.list(dir).unwrap().into_iter().map(|e| e.name).collect();
        n.sort();
        n
    }

    #[test]
    fn many_moves_and_copies_go_in_one_job_each_and_fail_one_by_one() {
        let fake = Fake::start();
        for i in 0..5 {
            fake.put_file(&format!("/A/f{i}.txt"), format!("{i}").as_bytes());
        }
        fake.put_file("/B/f4.txt", b"ja estava");
        let a = fake.account();
        a.list("/A").unwrap();
        a.list("/B").unwrap();
        let pairs: Vec<(String, String)> = (0..5).map(|i| (format!("/A/f{i}.txt"), format!("/B/f{i}.txt"))).chain([("/A/nada.txt".to_string(), "/B/nada.txt".to_string())]).collect();
        let before = fake.calls();
        let r = a.move_many(&pairs);
        assert_eq!(fake.calls_since(&before), ["files/move_batch_v2", "files/move_batch/check_v2", "files/move_batch/check_v2"]);
        assert!(r[..4].iter().all(Result::is_ok), "{r:?}");
        assert!(r[4].as_ref().unwrap_err().contains("Já existe"), "{r:?}");
        assert!(r[5].as_ref().unwrap_err().contains("Não encontrado"), "{r:?}");
        // The kept listings show it at once.
        assert_eq!(names(&a, "/A"), ["f4.txt"]);
        assert_eq!(names(&a, "/B"), ["f0.txt", "f1.txt", "f2.txt", "f3.txt", "f4.txt"]);
        let copies: Vec<(String, String)> = (0..4).map(|i| (format!("/B/f{i}.txt"), format!("/C/c{i}.txt"))).collect();
        let r = a.copy_many(&copies);
        assert!(r.iter().all(Result::is_ok), "{r:?}");
        assert_eq!(fake.content("/C/c2.txt"), b"2");
        assert!(fake.has("/B/f2.txt"));
    }

    #[test]
    fn entries_dropbox_was_too_busy_for_go_again() {
        let fake = Fake::start();
        for i in 0..4 {
            fake.put_file(&format!("/A/f{i}.txt"), b"x");
        }
        let a = fake.account();
        fake.busy_entries(2);
        let pairs: Vec<(String, String)> = (0..4).map(|i| (format!("/A/f{i}.txt"), format!("/B/f{i}.txt"))).collect();
        let before = fake.calls();
        assert!(a.move_many(&pairs).iter().all(Result::is_ok));
        assert_eq!(fake.count_since(&before, "files/move_batch_v2"), 2);
        // A single entry is answered at once; a change of case alone goes by move_v2.
        let before = fake.calls();
        let r = a.move_many(&[("/B/f0.txt".into(), "/B/F0.txt".into()), ("/B/f1.txt".into(), "/B/g1.txt".into())]);
        assert!(r.iter().all(Result::is_ok), "{r:?}");
        assert_eq!(fake.count_since(&before, "files/move_v2"), 3);
        assert_eq!(fake.count_since(&before, "files/move_batch_v2"), 1);
        assert!(fake.has("/B/g1.txt") && fake.has("/B/F0.txt"));
    }

    fn local_files(tag: &str, sizes: &[usize]) -> (PathBuf, Vec<PathBuf>) {
        let dir = std::env::temp_dir().join(format!("oe-dropbox-many-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let files = sizes
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let f = dir.join(format!("f{i}.bin"));
                std::fs::write(&f, (0..*n).map(|k| ((k * 7 + i) % 251) as u8).collect::<Vec<u8>>()).unwrap();
                f
            })
            .collect();
        (dir, files)
    }

    #[test]
    fn small_files_go_in_three_requests_and_the_same_ones_not_at_all() {
        let fake = Fake::start();
        let (dir, locals) = local_files("small", &[10, 0, 3000, 70_000, 5]);
        fake.put_file("/Dest/f2.bin", &std::fs::read(&locals[2]).unwrap());
        let a = fake.account();
        a.list("/Dest").unwrap();
        let files: Vec<(String, PathBuf)> = locals.iter().enumerate().map(|(i, l)| (format!("/Dest/f{i}.bin"), l.clone())).collect();
        let mut heard = vec![0u64; files.len()];
        let before = fake.calls();
        let r = a.upload_many(&files, &mut |i, n| {
            assert!(n >= heard[i]);
            heard[i] = n;
            true
        });
        assert!(r.iter().all(Result::is_ok), "{r:?}");
        assert_eq!(fake.calls_since(&before), ["files/upload_session/start_batch", "files/upload_session/append_batch", "files/upload_session/finish_batch_v2"]);
        for (i, l) in locals.iter().enumerate() {
            assert_eq!(fake.content(&format!("/Dest/f{i}.bin")), std::fs::read(l).unwrap());
            assert_eq!(heard[i], std::fs::metadata(l).unwrap().len());
        }
        assert_eq!(names(&a, "/Dest").len(), 5);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_busy_commit_goes_again_and_a_cancelled_file_stays_out() {
        let fake = Fake::start();
        let (dir, locals) = local_files("busy", &[100, 200_000, 300]);
        let a = fake.account();
        let files: Vec<(String, PathBuf)> = locals.iter().enumerate().map(|(i, l)| (format!("/D/f{i}.bin"), l.clone())).collect();
        fake.busy_entries(1);
        // 429 on the append: waited out, sent again (its progress heard from 0 again).
        fake.throttle_route("files/upload_session/append_batch");
        let r = a.upload_many(&files, &mut |i, n| !(i == 1 && n > 100_000));
        assert!(r[0].is_ok() && r[2].is_ok(), "{r:?}");
        assert_eq!(r[1].as_ref().unwrap_err(), CANCELLED);
        assert!(fake.has("/D/f0.bin") && fake.has("/D/f2.bin") && !fake.has("/D/f1.bin"));
        assert_eq!(fake.throttled(), 1);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_folder_cancelled_midway_stops_at_once() {
        let fake = Fake::start();
        let (dir, locals) = local_files("cancel", &[50_000; 6]);
        fake.make_dir("/F");
        let a = fake.account();
        a.list("/F").unwrap();
        let files: Vec<(String, PathBuf)> = locals.iter().enumerate().map(|(i, l)| (format!("/F/f{i}.bin"), l.clone())).collect();
        // Cancelar as the third file goes: the row all six belong to says stop from then on.
        let mut cancelled = false;
        let before = fake.calls();
        let r = a.upload_many(&files, &mut |i, n| {
            cancelled |= i == 2 && n > 0;
            !cancelled
        });
        assert!(r.iter().all(|r| r.as_ref().unwrap_err() == CANCELLED), "{r:?}");
        // The append stopped midway never reached Dropbox, nor did any other.
        assert_eq!(fake.calls_since(&before), ["files/upload_session/start_batch"]);
        assert!(a.list("/F").unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn batches_whose_answer_was_lost_are_checked_not_sent_again() {
        let fake = Fake::start();
        for i in 0..4 {
            fake.put_file(&format!("/A/f{i}.txt"), format!("{i}").as_bytes());
        }
        let a = fake.account();
        let pairs: Vec<(String, String)> = (0..4).map(|i| (format!("/A/f{i}.txt"), format!("/B/f{i}.txt"))).collect();
        fake.lose_answer("files/move_batch_v2");
        let before = fake.calls();
        let r = a.move_many(&pairs);
        assert!(r.iter().all(Result::is_ok), "{r:?}");
        assert_eq!(fake.count_since(&before, "files/move_batch_v2"), 1);
        assert_eq!(fake.count_since(&before, "files/move_v2"), 0);
        let copies: Vec<(String, String)> = (0..4).map(|i| (format!("/B/f{i}.txt"), format!("/C/f{i}.txt"))).collect();
        fake.lose_answer("files/copy_batch_v2");
        let before = fake.calls();
        assert!(a.copy_many(&copies).iter().all(Result::is_ok));
        assert_eq!(fake.count_since(&before, "files/copy_batch_v2") + fake.count_since(&before, "files/copy_v2"), 1);
        assert_eq!(fake.content("/C/f3.txt"), b"3");
        // Deletions: each item's last revision, for Desfazer.
        fake.lose_answer("files/delete_batch");
        let before = fake.calls();
        let gone: Vec<String> = (0..4).map(|i| format!("/C/f{i}.txt")).collect();
        let r = a.delete_many(&gone).unwrap();
        assert!(r.iter().all(|e| e.as_ref().is_ok_and(|e| !e.rev.is_empty())), "{r:?}");
        assert_eq!(fake.count_since(&before, "files/delete_batch"), 1);
        assert_eq!(fake.count_since(&before, "files/delete_v2"), 0);
        assert!(!fake.has("/C/f0.txt"));
    }

    #[test]
    fn an_upload_commit_whose_answer_was_lost_is_checked_by_its_bytes() {
        let fake = Fake::start();
        let (dir, locals) = local_files("lost", &[10, 3000, 70_000]);
        fake.put_file("/L/f1.bin", b"antes");
        let a = fake.account();
        let files: Vec<(String, PathBuf)> = locals.iter().enumerate().map(|(i, l)| (format!("/L/f{i}.bin"), l.clone())).collect();
        fake.lose_answer("files/upload_session/finish_batch_v2");
        let before = fake.calls();
        let r = a.upload_many(&files, &mut |_, _| true);
        assert!(r.iter().all(Result::is_ok), "{r:?}");
        assert_eq!(fake.count_since(&before, "files/upload_session/finish_batch_v2"), 1);
        assert_eq!(fake.count_since(&before, "files/upload_session/finish"), 0);
        for (i, l) in locals.iter().enumerate() {
            assert_eq!(fake.content(&format!("/L/f{i}.bin")), std::fs::read(l).unwrap());
        }
        // Never reached Dropbox: committed again, the same sessions.
        let files: Vec<(String, PathBuf)> = locals.iter().enumerate().map(|(i, l)| (format!("/M/f{i}.bin"), l.clone())).collect();
        fake.drop_before("files/upload_session/finish_batch_v2");
        let before = fake.calls();
        assert!(a.upload_many(&files, &mut |_, _| true).iter().all(Result::is_ok));
        assert_eq!(fake.count_since(&before, "files/upload_session/finish_batch_v2"), 2);
        assert_eq!(fake.count_since(&before, "files/upload_session/start_batch"), 1);
        assert_eq!(fake.content("/M/f2.bin"), std::fs::read(&locals[2]).unwrap());
        // As long as the batch may take: more than an RPC's minute and a half.
        assert!(crate::api::commit_timeout(UPLOAD_FILES) > std::time::Duration::from_secs(5 * 60));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_big_file_goes_alone_beside_a_batch() {
        let fake = Fake::start();
        let (dir, locals) = local_files("big", &[BATCH_FILE_MAX as usize + 10, 50]);
        let a = fake.account();
        let files: Vec<(String, PathBuf)> = locals.iter().enumerate().map(|(i, l)| (format!("/E/f{i}.bin"), l.clone())).collect();
        let before = fake.calls();
        assert!(a.upload_many(&files, &mut |_, _| true).iter().all(Result::is_ok));
        let calls = fake.calls_since(&before);
        assert!(calls.contains(&"files/upload_session/append_v2".to_string()) && calls.contains(&"files/upload_session/finish_batch_v2".to_string()), "{calls:?}");
        assert_eq!(fake.content("/E/f0.bin"), std::fs::read(&locals[0]).unwrap());
        let _ = std::fs::remove_dir_all(dir);
    }
}
