---
name: cloud-provider
description: Playbook for adding a cloud storage service or server protocol to Open Explorer (S3, Box, pCloud, Yandex, WebDAV presets, SharePoint sites…) or for making an existing one's listing, downloads or uploads faster. Use it whenever the work is a new provider, a new protocol, or the speed of one, so the lessons from Google Drive, OneDrive, Dropbox, MEGA, Backblaze B2 and Telegram are applied without being asked.
---

# Adding or speeding up a cloud provider

Open Explorer must beat Air Explorer in every benchmark of every provider it supports. Correctness comes first, then speed, and every speed claim is measured. This playbook is what the six providers already built taught; follow it without waiting to be told.

## Before writing code

1. **Pick the access route**, cheapest first:
   - A WebDAV, FTP or SFTP endpoint the provider documents → a preset over `crates/remote` (no new crate).
   - A standard protocol shared by many providers (S3) → one client crate, presets per provider.
   - An official REST API with self-service auth → a crate of its own.
   - No official API (reverse-engineered only) → stop and ask the user first.
2. **Read the official docs for the limits that decide speed**: page size and ordering of listings, batch endpoints, change/delta feeds, the biggest single upload, chunk rules (sizes, order, concurrency), `Range` support, rate limits and `Retry-After`, the content hash the API returns.
3. **Auth**: OAuth with the developer's own registration embedded, so users only click, log in and consent (see the `cloud-app-registrations` memory), with a JSON file or env var to override it. Use per-user credentials (Telegram's api_id) or keys (B2, S3) where the provider works that way. Never ask for secrets in chat.
4. Ask the user before creating test data in their accounts, unless they already allowed it for that provider.

## Building it

Follow the layout of the existing providers; `reference/integration.md` lists every touch point (crate files, `remote::Conn`, the app module, accounts, tree, settings, i18n, READMEs).

Design listing, downloads and uploads for speed from the start. `reference/performance.md` has the patterns with the code that does each and the numbers it bought. In short:

- **Listing**:
  - Show pages as they come (`Conn::list_pages`).
  - Ask only for the fields shown. One expensive field (OneDrive's `folder` childCount) cost 8 s per 1,000 folders.
  - Read big folders in parallel where ordering allows.
  - Keep listings on disk with a change-log position, so a folder opened before shows at once.
  - Read many folders in one request for tree walks.
- **Downloads**:
  - Big files go in parallel segments that take over slow or stalled ones (Drive's engine). Fixed segments lost to one stream on Dropbox and stalled, so measure before segmenting a new provider.
  - Resume with `Range`, write to a part file, check the hash.
- **Uploads**:
  - Use the biggest request the API allows.
  - Send chunks concurrently where the API allows it.
  - Send many small files in one batch.
  - Read retries from disk, not memory.
  - Reuse connections: one shared HTTP agent, never one per transfer.
- **Robustness**:
  - Retry with backoff only when a try made no progress.
  - Honour `Retry-After`.
  - Renew tokens on 401.
  - Make cancel answer at once.
  - Keep memory bounded: transfers × pieces in flight.

## Proving it

`reference/testing.md` covers the fake server, tests, live checks and the benchmark. The rules:

- **A fake server in the crate** that does what the real API does (Range, 429, dropped connections, batches, both list orders) and **a test for each behaviour and each bug**. A fix's test must fail without the fix; check that once.
- **Live checks through an example, not the app's GUI.** The user's screen is off limits unless they run or approve the benchmark.
- **Benchmark against Air Explorer** with `bench/compare.ps1`:
  - Add the provider to `$Clouds`.
  - Add its test data and upload watcher (`<provider>_bench` example: fill, `make`, `wait`, `clean`, `roundtrip`).
  - Compare downloads in alternating pairs.
  - Any measurement where Open Explorer is slower is a bug to fix before the work is done.
- **Every performance commit carries its before/after numbers** and how to rerun them.

## House rules that apply here

- Commits and PR text are in English, with no Claude attribution. Never push or merge unless asked. A merge commit ends with `(#N)`.
- UI strings go through `tr!("pt", "en")` / `t(…)`. User-facing docs go in both `README.md` and `README.pt-BR.md`.
- Never run `cargo fmt`: it rewrites dozens of unrelated files.
- Close the app before `cargo build`, or the link fails with "Acesso negado". An example running in the background (a bench waiter) blocks rebuilding that example too.
- Kill test servers (rclone) by PID only.
- `cargo clean` the checkout when the task that built is done.
- Air Explorer's free version moves 2 files at a time, so the harness sets Open Explorer to the same (`-AtOnce 2`) and restores the user's settings. If a run is interrupted, put the settings back yourself.
