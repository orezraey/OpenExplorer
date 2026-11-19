# Open Explorer

A dual-pane file manager for Windows with Google Drive, MEGA, SFTP, FTP and WebDAV built in.
Written in Rust.

[Português](README.pt-BR.md)

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
