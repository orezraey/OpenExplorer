# Speed patterns and what they bought

Each pattern names the code that does it, so a new provider can copy the shape. Numbers are from
`bench/compare.ps1` or the headless examples, on the user's PC and connection (about 1 Gbit/s).

## Listing

**Show pages as they arrive.** `Conn::list_pages` (default: one page) lets a pane show the first
1,000 items while the rest load: `remote/src/onedrive.rs` → `Account::list_paged`, and
`app/src/servers.rs` `load()` emits `Part::Page`. A provider whose listing pages implements it.

**Ask only for what's shown.** OneDrive's `folder` facet (childCount) costs Graph about 8 s per
1,000 folders. `onedrive/src/tree.rs` `LIST` selects `file` instead, and `Seen::listed` treats an
item with neither `file` nor `package` as a folder. Check each field's cost on a folder of
1,000 subfolders.

**Read a big folder from both ends.** Graph pages come one after the other, each about 1 s.
`onedrive/src/account.rs` `children_each` reads `$orderby=name` up and `name desc` down at once.
It stops when one way ends or the two meet, never at a kept count: a stale count left items out
(a review bug, fixed in `ed81883`). It falls back to plain paging when ordering is refused (400).

**Read many folders in one request for walks.**
- Drive: `gdrive/src/api.rs` `list_many` lists up to 50 folders with `'a' in parents or …`. The
  download of 100 folders went from 5.7 s to 0.43 s; Air Explorer takes 2.3 s.
- OneDrive: `Account::read_ahead` lists every unlisted subfolder, 20 per `$batch`, 4 batches at
  once. `Conn::read_ahead` is called by the app's `measure()` before a folder download. Before
  it, 100 folders put nothing on disk in 30 s.
- A work queue takes several queued folders at once (`app/src/work.rs` `Queue::take`).
- When the same folder is queued twice, each row needs its own copy of the listing (`ed81883`).

**Keep listings between runs, with a change-log position.**
- Drive: `app/src/cache.rs` keeps listings with a `Pos`, `app/src/watch.rs` reads the change log
  every 20 s and `verdict()` proves a cached listing unchanged with one small request.
- OneDrive: a tree (`onedrive/src/tree.rs`) fed by the delta feed (`Account::catch_up` in
  `onedrive/src/account.rs`).
- Dropbox: a recursive cursor and longpoll (`dropbox/src/cache.rs`).
- MEGA: the whole tree in memory, with `sc` polling.
- Result: a folder opened again shows in about 0.03 s, against Air Explorer's 0.16 s. First
  opens: Drive 5,000 files 4.7 s against 20.3 s; OneDrive 6.6 s against 37.3 s.
- Anything shortcut from cached data must be vouched for by that log: see `Watch::vouches`, used
  by the address bar's typed paths since `effd144`.

## Downloads

**Segments that take over slow ones** (`gdrive/src/download.rs`):
- A big file (from 64 MiB, setting `big_files_in_segments`) comes in 8 `Range` requests.
- A segment done with its part takes the end of the one that would finish last at its speed
  (`take_over`). The share is sized so both end together, counting the new request's wait of
  about 0.6 s.
- A request silent for 2 s (6 s for its answer) is asked again from where it got to.
- Each request is read on a thread of its own (`ask`), so a stalled read never holds the
  download.
- 500 MiB: median 7.72 s to 6.12 s, worst 125.7 s to 7.3 s. 2 GiB: 19.9 s against Air Explorer's
  33.1 s.

**Don't segment blindly.** Dropbox with fixed segments lost every round to one stream and
stalled once for 128 s; it downloads in one stream. `Account::download_split` exists, tested
but unused. Segment a new provider only after an alternating A/B shows a gain, and then with
Drive's take-over engine, ideally moved to a shared crate rather than copied.

**Always:** resume with `Range` from the bytes written, of the same version (Dropbox pins
`rev:`). Write to a part file with a name of its own. Check the provider's hash (QuickXor,
Dropbox content hash, MEGA MAC). Make cancel answer at once.

## Uploads

**Biggest request the API allows.**
- OneDrive sessions take parts "less than 60 MiB", multiples of 320 KiB, one after the other:
  56.25 MiB parts took 100 MiB from 12.9 s to 7.5 s.
- Dropbox `files/upload` takes up to 150 MiB in one request: `upload_file` sends the file read
  from disk again for each try (`Body::File`) and hashes it meanwhile. 100 MiB went from 9.8 s
  to about 3 s, against Air Explorer's 4.9 s.

**Concurrent chunks where the API allows them.**
- MEGA: `mega/src/upload.rs` sends pieces of whole MAC chunks up to 8 MiB, 6 at once. The MAC
  and encryption run in order on the reading thread. 100 MiB: 27.9 s to 4.9 s.
- Dropbox above 150 MiB: a `concurrent` session, 6 blocks of 8 MiB at once, the closing block
  last.
- A worker must not hold a lock while it sends. `while let Ok(x) = rx.lock().recv()` keeps the
  guard for the whole loop body, which serialised Dropbox's senders. Bind it in a `let … else`.

**Many small files in one batch.** Dropbox `batch.rs` `upload_many` (exposed by `Conn::batches`
and `upload_files`): 100 files took 6.0 s against Air Explorer's 41 s.

**Reuse connections.** One shared HTTP agent per provider, never one per transfer
(`mega/src/api.rs` `transfers()`). A 512 KiB MEGA file took 2 s on a new connection and 0.65 s on
a kept one, and 100 files went from 127 s to 66 s.

**Bound memory.** Every buffer in flight is multiplied by the transfers at once (up to 10).
OneDrive holds one 56.25 MiB part per upload; MEGA and Dropbox up to about 70 MB of pieces per
file. Say the worst case in the commit.

## Robustness

- Retry with backoff only while tries make no progress, and give up after a few such tries.
  When a try made progress, ask again at once.
- Honour 429 and 503 `Retry-After`. Renew the token on 401 and try again once.
- A blocking read needs a timeout. A stalled request should be abandoned, not waited for.
- Cancel stops every thread soon, background hashing included (`ed81883`).
