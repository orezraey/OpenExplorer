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

## Building from source

Install Rust with [rustup](https://rustup.rs). The toolchain version is pinned in
`rust-toolchain.toml`.

```
cargo build --release
cargo test --workspace
```

The UI framework, [Rune](https://github.com/anomalyco/rune), is used from its `dev` branch and
pinned in `Cargo.lock`.
