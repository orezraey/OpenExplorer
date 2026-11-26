# Open Explorer

A dual-pane file manager for Windows with Google Drive, MEGA, SFTP, FTP and WebDAV built in.
Written in Rust.

[Português](README.pt-BR.md)

<table align="center">
  <tr>
    <td align="center" width="200"><h1>37 of 37</h1><b>Benchmarks won</b><br><sub>against Air Explorer 6.0, on the same PC and connection</sub></td>
    <td align="center" width="200"><h1>4.8×</h1><b>Less time</b><br><sub>the median of the 36 timed measurements</sub></td>
    <td align="center" width="200"><h1>0.17 s</h1><b>To start</b><br><sub>Air Explorer takes 3.1 s</sub></td>
    <td align="center" width="200"><h1>189 MB</h1><b>Memory, idle</b><br><sub>Air Explorer uses 240 MB</sub></td>
  </tr>
</table>

## Compared with Air Explorer

**Faster than Air Explorer in all 37 measurements: opening folders, downloading, uploading,
starting and memory.**

<picture>
  <source media="(prefers-color-scheme: dark)" srcset="docs/bench/speed-en-dark.svg">
  <img src="docs/bench/speed-en-light.svg" alt="Time to finish nine tasks, Open Explorer against Air Explorer: from 1.1 to 25 times faster." width="880">
</picture>

Measured on 25 and 26 November 2026, on one Windows 11 PC and one internet connection of about
1 Gbit/s, against the free version of Air Explorer 6.0.0. Both apps moved 2 files at a time, the
free version's limit. The script [`bench/compare.ps1`](bench/compare.ps1) drives both apps
through Windows UI Automation, as a screen reader would, so neither app was changed, and anyone
can run it again. Each number is the median of several runs; downloads were timed in
alternating pairs, because the connection's speed changes from minute to minute. The closest
result, a 500 MiB download from Dropbox, won 4 of 5 alternating rounds rather than all of them.

Open Explorer's time in bold, then Air Explorer's:

| | Google Drive | OneDrive | Dropbox | MEGA |
| --- | --- | --- | --- | --- |
| Open a folder of 5,000 files, first time | **4.7 s** · 20.3 s | **6.6 s** · 37.3 s | **2.7 s** · 6.0 s | **0.03 s** · 0.18 s |
| Open a folder of 1,000 folders, first time | **1.1 s** · 4.0 s | **0.03 s** · 12.1 s | **0.55 s** · 1.17 s | **0.03 s** · 0.17 s |
| Download a 500 MiB file | **6.1 s** · 8.3 s | **16.2 s** · 80.2 s | **7.9 s** · 8.5 s | **9.9 s** · 20.2 s |
| Download 100 folders | **0.47 s** · 2.3 s | **0.09 s** · 2.3 s | **0.10 s** · 2.4 s | **0.10 s** · 2.4 s |
| Upload a 100 MiB file | **4.6 s** · 7.3 s | **7.5 s** · 12.8 s | **3.0 s** · 4.9 s | **4.9 s** · 11.0 s |
| Upload 100 files of 512 KiB | **79 s** · 109 s | **44 s** · 87 s | **6.0 s** · 41 s | **66 s** · 108 s |

| | Open Explorer | Air Explorer 6.0 (free) |
| --- | --- | --- |
| Start | **0.17 s** | 3.1 s |
| Memory, idle | **189 MB** | 240 MB |
| Open `C:\Windows\System32` (5,040 items) | **0.03 s** | 0.21 s |
| Open `C:\Windows\WinSxS` (28,491 items) | **0.16 s** | 1.09 s |
| Open a cloud folder of 5,000 files again | **0.03 to 0.06 s** | 0.16 to 0.17 s |
| Download a 2 GiB file from Google Drive | **19.9 s** | 33.1 s |
| Accounts in each cloud | **As many as you like** | One |
| Files moved at once | **You choose, 1 to 10** | 2 |

Every measurement, with how it was taken, is in
[`bench/results/comparison.csv`](bench/results/comparison.csv); how to run the comparison is in
[`bench/README.md`](bench/README.md).

## Features

* Two panes, each with its own folder tree.
* Google Drive: multiple accounts, shared drives, sharing, starred, trash. Open folders update
  when files change on the web.
* MEGA: multiple accounts (two-factor login too), Cloud Drive, Rubbish Bin with restore,
  incoming shares and public file links. Encrypted and decrypted on your computer; open folders
  update when files change elsewhere.
* OneDrive: personal and work or school accounts, sharing links, recycle bin with undo (and, on
  work accounts, the Lixeira through SharePoint). Open folders update when files change
  elsewhere.
* Dropbox: personal and team accounts, shared links, deleted items with undo. Open folders
  update when files change elsewhere.
* Backblaze B2: buckets and folders, large uploads in parallel parts (and resumed), hidden
  versions with undo.
* SFTP, FTP/FTPS, WebDAV, Nextcloud and ownCloud.
* Ready to add through WebDAV: Koofr, pCloud, Yandex Disk, Mail.ru Cloud, MagentaCLOUD,
  HiDrive (STRATO and IONOS), kDrive, Stack, InfiniCLOUD, Mail.com Cloud and Onlime limeDRIVE.
  Pick the service, then type your login: the server and folder are filled in, and the form
  says which password to use (several ask for an app password made on their website).
* Copy and move between any of them, with a transfer queue.
* Instant file name search on local NTFS drives, similar to Everything. Drive, MEGA and servers
  can be searched too.
* Drag and drop to and from Windows Explorer.
* Undo and redo for renames, moves, deletes and copies.
* English and Portuguese.

## Installation

Download the latest version from [Releases](../../releases): the `setup.exe` installs Open
Explorer with a Start menu shortcut and an uninstaller, and the `portable.exe` runs without
installing. Neither is code-signed yet, so Windows SmartScreen will warn the first time.

Linux and macOS builds are provided but experimental. Computer search isn't available on macOS.

The first local search offers to install the search service, which needs administrator rights
once. To remove it, run as administrator:

```
"%ProgramFiles%\OpenExplorer\openexplorer-index.exe" index uninstall
```

## Google Drive

Until the app passes Google's verification, you need your own OAuth client (free):

1. Create a project in the [Google Cloud console](https://console.cloud.google.com) and enable
   the Google Drive API.
2. Set up the OAuth consent screen as External, in Testing, with your account as a test user.
3. Create an OAuth client ID of type Desktop app and download the JSON.
4. In Open Explorer, go to Add account → Google Drive and paste the JSON.

In Testing mode Google signs you out after 7 days.

## OneDrive

Add account → OneDrive opens Microsoft's sign-in in the browser: pick the account (personal,
work or school), allow Open Explorer to access your files, and that's it.

Microsoft Graph doesn't list a work or school account's recycle bin, nor restore from it, so
Open Explorer reads it through SharePoint's REST API. Open the account's Lixeira and click
Authorize in browser (some organizations need an administrator to grant consent); from then on
the Lixeira lists what was deleted, where from and when, with Restore, Delete permanently and
Empty trash, and Undo brings back what Open Explorer deleted. Without it the account works as
before, without the Lixeira. Personal accounts have no Lixeira (Graph doesn't list theirs); Undo
restores what was deleted. The Personal Vault only opens on Microsoft's website or app.

To use an app registration of your own instead (Microsoft Entra, "Accounts in any organizational
directory and personal Microsoft accounts", platform "Mobile and desktop applications" with
`http://localhost`, public client flows on, Graph `Files.ReadWrite.All`, `offline_access`,
`User.Read`, and SharePoint `AllSites.Write`), save `{"client_id": "…"}` to
`%APPDATA%\OpenExplorer\onedrive-client.json` or set `OPENEXPLORER_ONEDRIVE_CLIENT_ID`.

## Dropbox

Add account → Dropbox opens Dropbox's sign-in in the browser: allow Open Explorer, and that's it.

To use an app of your own instead (Dropbox App Console: Scoped access, Full Dropbox; scopes
`account_info.read`, `files.metadata.*`, `files.content.*`, `sharing.*`; redirect URI
`http://localhost:53719`), save `{"app_key": "…"}` to
`%APPDATA%\OpenExplorer\dropbox-client.json` or set `OPENEXPLORER_DROPBOX_APP_KEY`.

## Backblaze B2

Make an application key under Application Keys on the Backblaze website, for every bucket or
just one (and, if you like, just a prefix). Seeing and downloading needs `listBuckets`,
`listFiles` and `readFiles`; uploading, making folders, renaming and deleting needs
`writeFiles`; Delete every version needs `deleteFiles`. Then Add account → Backblaze B2 with the
key ID and the application key (shown only once on the website).

## Building from source

Install Rust with [rustup](https://rustup.rs). The toolchain version is pinned in
`rust-toolchain.toml`.

```
cargo build --release
cargo test --workspace
```

The UI framework, [Rune](https://github.com/anomalyco/rune), is used from its `dev` branch and
pinned in `Cargo.lock`.
