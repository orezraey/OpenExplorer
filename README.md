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

Personal and work or school accounts sign in through the browser with an app registration of
your own (free, no secret):

1. In the [Microsoft Entra portal](https://entra.microsoft.com), App registrations → New
   registration, account types "Accounts in any organizational directory and personal
   Microsoft accounts".
2. Authentication: add the platform "Mobile and desktop applications" with the redirect URI
   `http://localhost`, and turn on "Allow public client flows".
3. API permissions: Microsoft Graph, delegated, `Files.ReadWrite.All`, `offline_access` and
   `User.Read`. For the recycle bin of work or school accounts, also SharePoint, delegated,
   `AllSites.Write` (see below).
4. In Open Explorer, go to Add account → OneDrive and paste the Application (client) ID (it's
   kept in `%APPDATA%\OpenExplorer\onedrive-client.json`; `OPENEXPLORER_ONEDRIVE_CLIENT_ID`
   works too).

Microsoft Graph doesn't list a work or school account's recycle bin, nor restore from it, so
Open Explorer reads it through SharePoint's REST API, which needs that SharePoint permission.
With it in the registration, open the account's Lixeira and click Authorize in browser (or have
an administrator grant consent for the organization); from then on the Lixeira lists what was
deleted, where from and when, with Restore, Delete permanently and Empty trash, and Undo brings
back what Open Explorer deleted. Without it the account works as before, without the Lixeira.
Personal accounts have no Lixeira (Graph doesn't list theirs); Undo restores what was deleted.

## Building from source

Install Rust with [rustup](https://rustup.rs). The toolchain version is pinned in
`rust-toolchain.toml`.

```
cargo build --release
cargo test --workspace
```

The UI framework, [Rune](https://github.com/anomalyco/rune), is used from its `dev` branch and
pinned in `Cargo.lock`.
