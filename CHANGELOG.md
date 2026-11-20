# Changelog

## 0.3.0 - 2026-11-19

### Added

* OneDrive accounts, personal and work or school. Sign in through the browser and allow Open
  Explorer: nothing to register or paste. Copy, move, rename, delete to the recycle bin with
  undo, sharing links and the storage in the status bar. On work and school accounts the
  Lixeira lists what was deleted, where from and when, with Restore, Delete permanently and
  Empty trash. Open folders update when something changes elsewhere.
* Dropbox accounts, personal and team. Sign in through the browser, nothing to paste. Copies,
  moves, deletes and uploads of many items go in batches; deleting can be undone, folders
  included, and "Deleted items…" on a folder lists what was deleted there to bring it back.
  Shared links, the storage in the status bar, and open folders that update when something
  changes elsewhere.
* Backblaze B2 keys. Buckets and folders, with the size of each shown without opening it.
  Large uploads go in parallel parts and pick up where they stopped; "Unfinished uploads…"
  on a bucket shows, resumes or cancels them. Deleting hides the files and can be undone;
  "Delete every version" deletes for good.
* Options › Cache shows how much room the caches take and clears them.

### Changed

* MEGA accounts start much faster: the decrypted tree is kept on disk, sealed, and a start
  reads only what changed. It also takes about a fifth of the memory.
* Reopening a Google Drive folder that didn't change no longer reads it again.
* The folder tree shows a cloud account's folders at once from what the account keeps.

### Fixed

* After F2, a click on another item left the rename open and the arrow keys stopped working
  in both panes.
* A transfer's progress bar could jump when a part was sent again.

## 0.2.0 - 2026-11-18

### Added

* MEGA accounts. Sign in with e-mail and password, with two-factor codes too, and add as many
  accounts as you like. Browse Cloud Drive, Rubbish Bin and folders shared with you; create
  folders, rename, delete to the Rubbish Bin and restore. Copy and move between MEGA, your
  computer, Google Drive, servers and other MEGA accounts, drag and drop to and from Windows
  Explorer, undo, search, and public links to files. Files are encrypted and decrypted on your
  computer, and open folders update when something changes elsewhere.

### Changed

* The transfer queue shows why a transfer failed, in the row itself, instead of only "Error".

### Fixed

* A batch on a server (move, cut and paste, delete) no longer loses track of items whose path
  changes while it runs.

## 0.1.0 - 2026-11-18

First release.
