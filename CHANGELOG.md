# Changelog

## 0.5.1 - 2026-11-26

### Fixed

* Uploads and downloads at the same time keep to the numbers set in Options › Transfers. Each
  new copy started while another ran brought as many again, so two batches of 4 uploads sent 8
  files at once. The limit now counts every batch together, on Google Drive and on the other
  clouds and servers, and a change to it applies at once.
* A transfer cancelled while it waits for another to finish ends at once.

## 0.5.0 - 2026-11-26

### Added

* One, two or four panes, from View in the toolbar. Four show in a 2×2 grid, each with its own
  tabs, tree and history.
* Each pane shows its files as Explorer does: Extra large, Large or Medium icons, List or
  Details, from the button at the right of the pane's bar. A click on the icon goes to the next
  view, the arrow beside it picks one.
* Open Explorer updates itself. Once a day it looks for a new release on GitHub, downloads it in
  the background and installs it at the next start, or at once with Restart to update. Each
  release is signed, and the signature is checked again just before installing. Options ›
  General has Update automatically and Check for updates.
* Google Drive works without a client of your own: Add account → Google Drive → Connect in
  browser. Until Google verifies the app, it shows a warning (Advanced → Go to Open Explorer).
  A client of your own still works, and each account keeps the one it signed in with.
* Thirteen clouds that speak WebDAV are ready to add, with their logos: Koofr, InfiniCLOUD,
  MagentaCLOUD, Mail.ru Cloud, Yandex Disk, Stack, Onlime limeDRIVE, HiDrive (STRATO and IONOS),
  kDrive, pCloud US and EU, and Mail.com Cloud. You type only your login.
* Animations, each with its own switch in Options › Animations: the hover highlight glides from
  row to row, copied or cut items swing for a moment and cut ones fade, the keyboard's
  selection glides, rows slide to their place when the list is sorted, and the tree's arrows
  turn. Rows can have stripes.
* Zoom in Details with Ctrl and the wheel, Ctrl+= and Ctrl+-, or a pinch; Ctrl+0 or the
  percentage in the status bar goes back to 100%.
* Details' columns can be put in another order by dragging their header.
* macOS: Cmd+Q, Quit in the Dock and logging out ask first while transfers run, as closing the
  window does.
* Linux: a right click in a text field shows Cut, Copy and Paste, also in the add-account
  window.
* Options › Experimental: an acrylic background on macOS and Linux, and on Windows, content
  that keeps up while the window's left or top edge is dragged.
* `openexplorer --version` prints the version.

### Changed

* Open Explorer is faster than Air Explorer in all 37 measurements of an automated comparison,
  with a median of 4.8 times less time. The README shows the figures.
* Big Google Drive downloads come in 8 parallel parts. A part that ends takes over the rest of
  a slow or stalled one.
* A Drive folder being downloaded reads its subfolders many at a time.
* Big OneDrive folders are read from both ends and show page by page. A OneDrive folder
  download reads its folders ahead, and uploads go in 56.25 MiB parts.
* MEGA uploads go in pieces, several at once: 100 MiB went from 27.9 s to 4.9 s.
* Dropbox sends a file up to 150 MiB in one request: 100 MiB went from 9.8 s to 3.0 s.
* Context menus are spaced as Windows 11's, with a thin border.
* The field menus and the window buttons' names for screen readers are in Portuguese while the
  interface is.
* List and the hover highlight cost the window far less: a list of 2,000 items in List view
  takes about 7 times less time per update while transfers run.

### Fixed

* A file downloaded twice into one folder at once failed with "The system cannot find the file
  specified"; both copies finish now.
* A download with a Drive folder and a shortcut to it saved the second one empty.
* A typed Drive path could lead to a folder renamed, moved or deleted elsewhere.
* Cancelling a Dropbox upload no longer waits for the whole file to be read.
* macOS and Linux: a window closed on a display that is gone opens in the middle of the screen.

## 0.4.0 - 2026-11-25

### Added

* Telegram accounts. Files are kept in a private channel of your own account, each folder a
  message and each file a document, as Telegram-Drive does. You register your own app at
  my.telegram.org and type its api_id and api_hash once; they stay in Windows' credential
  store. Copy, move, rename and delete (for good: Telegram has no bin), over up to 8
  connections at once with Premium (4 on a free account). "Telegram chats", at the top of the
  account, shows Saved Messages, chats, groups and channels, read-only, with their files by
  month.
* Tabs in each pane. Ctrl+T or + opens a picker with This PC, Documents, Pictures and every
  connected account and server; Ctrl+W closes a tab. Each tab keeps its folder, history,
  selection and search.
* An Accounts tab beside Explore lists every account and server with its storage, filters them
  by provider or by name, and opens, edits or removes each one.
* Adding or editing an account opens a window of its own, with Personal, Business and NAS tabs
  and a search, and the main window stays usable beside it.
* A right-click menu on the folder tree: Expand/Collapse, Cut, Copy, Paste, Delete, Rename, New
  folder and Properties.
* Buttons in each pane to hide the folder tree and to make a new folder.
* F5 reloads the folder, or every account's storage on the Accounts tab. The mouse's back and
  forward buttons step through a pane's history.
* Options › Experimental: a translucent background on Windows 11 (Mica, Mica Alt or Acrylic)
  around the panes, in the folder tree and in the status bar, and a highlighter on search
  results.

### Changed

* Icons are Windows' own at the size Explorer draws them, This PC's and those of special
  folders such as Documents and Downloads included.
* The main window has a title bar of its own, and reopens where it was closed, maximized if it
  was.
* Closing the window while transfers run asks first.
* The file list and the folder tree scroll sideways, and long names in the tree are no longer
  cut.
* The Desktop starts collapsed in the folder tree.
* While a folder loads, the pointer shows the busy circle, as in Explorer.
* The transfer queue takes about 6.5 times less CPU while transfers run.
* The first launch after installing starts about a second sooner.
* With the Automatic theme, switching Windows between light and dark is followed at once.

### Fixed

* A folder with thousands of subfolders froze the window while their icons were looked up:
  C:\Windows\WinSxS took 1.76 s, now 8 ms.
* The mouse wheel over a scroll bar scrolls again.
* A search that was left stops showing results at once.

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
