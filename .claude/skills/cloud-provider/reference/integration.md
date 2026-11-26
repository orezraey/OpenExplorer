# Where a provider plugs in

Copy the newest provider closest in kind. Backblaze B2 and Dropbox are the fullest API-based
examples, and MEGA covers client-side crypto. Read the matching files before writing.

## The provider's own crate: `crates/<provider>/`

HTTP providers use a blocking `ureq` 3 client, with no async runtime (Telegram speaks MTProto
instead):

| File | Holds |
|---|---|
| `lib.rs` | errors, built-in app ids |
| `api.rs` | HTTP agents and calls, errors read as sentences, `Retry-After` |
| `oauth.rs` | login in the browser on a localhost redirect, with PKCE where offered |
| `store.rs` | the account list (JSON in `%APPDATA%\OpenExplorer`), with tokens in Credential Manager |
| `account.rs` | `reach(key)` to open a stored account; list, stat, mkdir, rename, move, delete and restore, quota, links |
| `transfer.rs` (or `download.rs` / `upload.rs`) | downloads and uploads |
| `cache.rs` | listings kept between runs, sealed (`openexplorer-cloud` `sealed`), plus the change feed |
| `fake.rs` | an in-process fake of the API for tests (B2's is `mock.rs`; Drive has small TCP fakes inside each module's tests) |
| `tests/real_account.rs` | `#[ignore]` tests against a real account |
| `examples/<provider>_bench.rs` | benchmark data and upload watcher (see `testing.md`) |

Shared plumbing lives in `crates/cloud` (`each` for work in parallel, `fill`, `sealed`). Put a
helper there when a second provider needs it.

## The connection the panes use: `crates/remote`

- Add a `Protocol` variant, kept out of `servers.json` like MEGA, Dropbox and B2.
- `remote/src/<provider>.rs` implements `Conn`. Required: `list`, `stat`, `mkdir`, `rename`,
  `remove_file`, `remove_dir`, `download`, `upload`, `home`, `is_link`, `alive`.
- Then the optional methods that make it fast or exact:

| Method | When to implement it |
|---|---|
| `list_pages` | the provider lists in pages |
| `read_ahead` | the provider can list many folders in one request |
| `upload_file` | the provider can send a local file better than through a `Read` (one request, dedupe by hash) |
| `upload_files` + `batches` | the provider has batch uploads |
| `copy`, `copy_many`, `rename_many` | the provider does server-side copies or moves in batches |
| `remove` | a whole folder goes in one call |
| `passed_over` | a walk should skip a folder (Telegram's chats) |
| `pin` | a stable id that survives renames |

- Progress must never go back: wrap the callback in `onward`.

## The app: `crates/app/src/`

- `<provider>.rs`: what only this provider has. Adding an account (dialog, OAuth), its delete
  and undo semantics (recycle bin or permanent), deleted items, shared links, quota in the status
  bar, and refreshing open folders on changes (longpoll or poll).
- `servers.rs`: listing, transfers, `measure()` and `fetch()`. A provider works through it like a
  server, so most features come free.
- `accounts.rs`, the tree, places and menus: a provider shows under Contas like the others. Grep
  for `Protocol::B2` to find every spot.
- `settings.rs`: transfers at once (`uploads`, `downloads`, up to 10) and
  `big_files_in_segments`.
- Strings go through `tr!("pt", "en")` and `t("pt", "en")` from `openexplorer_i18n`.

## Docs

- `README.md` and `README.pt-BR.md`: a Features line, plus a section on adding the account. Say
  what users register themselves, if anything, and how to use their own app id.
- `bench/README.md`: the provider's test data command.
- Keep the README comparison with Air Explorer current. Its data lives in
  `bench/results/comparison.csv`, and `bench/readme-chart.ps1` redraws the chart.
