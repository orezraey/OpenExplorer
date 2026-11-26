# Speed comparison with Air Explorer

`compare.ps1` times Open Explorer and Air Explorer doing the same things, the same way: it drives
both through Windows UI Automation, as a screen reader would, so nobody counts seconds and neither
app is changed or taken apart. Air Explorer's licence (https://www.airexplorer.net/en/terms/) has
no clause against publishing benchmarks.

## What it measures

Each measurement runs once untimed, then `-Runs` times (5 by default); the median is reported.

| Measurement | From | Until |
|---|---|---|
| `start` | the app is launched | its left pane shows its first folder whole |
| `system32`, `winsxs` | Enter in the left pane's address bar, after `C:\Windows\System32` (5,040 items) or `C:\Windows\WinSxS` (28,491) | the pane shows every item |
| `<cloud>-1000-folders`, `<cloud>-5000-files` | Enter in the cloud pane's address bar, after `Open Explorer benchmark/1000 folders` (or `5000 files`), a folder the app opened before in this session | the pane shows every item |
| `<cloud>-1000-folders-uncached`, `<cloud>-5000-files-uncached` | a double click on the folder in `Open Explorer benchmark`, right after the app started with no listing kept from before | the pane shows every item |
| `download-large` | the arrow between the panes that copies the cloud pane's selection (all of `transfer/large`: one 500 MiB file) into the other pane's empty local folder | the file is on disk whole and no program still writes it |
| `download-huge` | the same, for Drive's `transfer/huge` (one 2 GiB file), long enough that the line's ups and downs average out | the same |
| `download-100-folders` | the same, for `100 folders` (100 empty folders) | all 100 folders are on disk |
| `upload-large`, `upload-100-files` | the arrow that copies the left pane's selection (all of `C:\OEBench\up\large`, one file of 100 MiB, or `up\small`, 100 of 512 KiB, made once with random bytes) into an empty cloud folder under `Open Explorer benchmark/uploads` | the cloud holds every file (asked every 200 ms by the cloud's example: `upload_target` for Drive, the `*_bench` ones for the others; a cloud shows a file only once its upload finished) |

The transfers are Google Drive's as named above; the other clouds' start with the cloud's name
(`onedrive-download-large`, `mega-upload-100-files`). Open Explorer moves `-AtOnce` files at once
each way while measured (2 by default, as Air Explorer's free version does): the script sets it
in Open Explorer's settings and puts them back at the end. A run that fails is told and ends its
measurement; the others go on.
| `memory` | 10 s idle after a start, then after opening the Drive folder | private bytes and working set of the app's processes |

`<cloud>` is `drive` (Google Drive), `onedrive`, `dropbox` or `mega`: each cloud whose account
Open Explorer has connected.

Between runs a pane goes back to another folder (`C:\Windows\Web`, or the benchmark's top folder
in Drive), so each run opens its folder again. "Shows every item" is read from each app's own
list: Open Explorer's status bar ("5.040 itens") and Air Explorer's file list, a standard Windows
list view (`LVM_GETITEMCOUNT`). Open Explorer may still be checking a cloud folder with Google
while its items are all on screen ("· carregando…"); that counts as shown, since Air Explorer
shows what it kept without checking.

## Running it

1. Build Open Explorer: `cargo build --release -p openexplorer`.
2. Connect the same cloud accounts in both apps, one per cloud (Air Explorer's free version takes
   no more). In Air Explorer, keep This PC in the left pane and open a tab for each account in the
   right one (each of its tabs belongs to one account); the script brings each forward.
3. Make the test data, once per cloud (run again, each adds only what's missing):
   - Google Drive: `cargo run -p openexplorer-gdrive --release --example bench_data -- <e-mail>`
     (it needs about 2.8 GB free in the account; `--huge` makes only the 2 GiB file, skipped without room for it). In an account with no room left, `--shortcuts`
     fills `5000 files` with shortcuts, which take no storage, and moves a leftover
     `openexplorer-speed-500.bin` into `transfer/large`.
   - OneDrive: `cargo run -p openexplorer-onedrive --release --example onedrive_bench -- <drive id>`
     (the id is in `onedrive-accounts.json`, in Open Explorer's settings folder).
   - Dropbox: `cargo run -p openexplorer-dropbox --release --example dropbox_bench -- <e-mail>`.
   - MEGA: `cargo run -p openexplorer-mega --release --example mega_bench -- <e-mail>`.

   Each of those three needs about 0.6 GB free, and makes `transfer/large` and `uploads` besides
   the folders. Build the examples before a run that uploads (`cargo build --release --examples
   -p openexplorer-gdrive -p openexplorer-onedrive -p openexplorer-dropbox -p
   openexplorer-mega`): the script runs them to make the uploads' folders, watch them fill and
   delete them.
4. Run, and leave the mouse and keyboard alone while both apps open and close:

   ```
   powershell -NoProfile -ExecutionPolicy Bypass -File bench\compare.ps1
   ```

   The accounts are read from Open Explorer's settings folder (`-Email` picks the Google one when
   there are several). `-Only start,winsxs` runs some measurements; `-Apps airexplorer` one app.
   Results go to `bench\results\`.

The `-uncached` measurements close the app before each run and delete the listings Open Explorer
keeps on disk (`%LOCALAPPDATA%\OpenExplorer`: `cache` for Google Drive, and a folder per cloud);
Air Explorer keeps none. They open the folder with a double click, as a person would: through its
address bar Air Explorer took about 40 s for a OneDrive folder it hadn't listed yet.

Each download goes to a folder of its own (`C:\OEBench\dl-*`) and each upload to a Drive folder
of its own (`Open Explorer benchmark/uploads/up-*`), all made before each app starts: Air
Explorer doesn't see a folder made after it listed the one holding it. Nothing is emptied while
an app may still write to it; the folders go at the end, the Drive ones for good. The other app
is closed while one is measured, so its window can't take the clicks. Run one copy of the
script at a time.

Air Explorer keeps unfinished transfers and resumes them at its next start: if a run is cut short
while it copies, cancel what's left in its queue (select the rows, Shift+End, right click,
Cancelar) before running again.

`uia-dump.ps1` (an app's UI Automation tree) and `list-count.ps1` (Air Explorer's list counts) are
the read-only tools the script was written with. The examples `drive_ls` (a Drive folder's
contents and sizes) and `download_speed` (one file in one stream and in segments) helped choose
the test data and how big files come down.
