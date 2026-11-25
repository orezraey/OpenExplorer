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
| `drive-1000-folders`, `drive-5000-files` | Enter in the Google Drive pane's address bar, after `Open Explorer benchmark/1000 folders` (or `5000 files`) | the pane shows every item |
| `download-large` | the arrow between the panes that copies the Drive pane's selection (all of `transfer/large`: one 500 MiB file) into the other pane's empty local folder | the file is on disk whole and no program still writes it |
| `download-100-folders` | the same, for `100 folders` (100 empty folders) | all 100 folders are on disk |
| `memory` | 10 s idle after a start, then after opening the Drive folder | private bytes and working set of the app's processes |

Between runs a pane goes back to another folder (`C:\Windows\Web`, or the benchmark's top folder
in Drive), so each run opens its folder again. "Shows every item" is read from each app's own
list: Open Explorer's status bar ("5.040 itens") and Air Explorer's file list, a standard Windows
list view (`LVM_GETITEMCOUNT`). Open Explorer may still be checking a cloud folder with Google
while its items are all on screen ("· carregando…"); that counts as shown, since Air Explorer
shows what it kept without checking.

## Running it

1. Build Open Explorer: `cargo build --release -p openexplorer`.
2. Connect the same Google account in both apps. In Air Explorer, keep This PC in the left pane and
   the Google Drive account in the right one (each of its tabs belongs to one account).
3. Make the Drive test data, once: `cargo run -p openexplorer-gdrive --release --example bench_data -- <e-mail>`
   (it needs about 400 MB free in the account; run again, it adds only what's missing). In an
   account with no room left, `--shortcuts` fills `5000 files` with shortcuts, which take no
   storage, and moves a leftover `openexplorer-speed-500.bin` into `transfer/large`.
4. Run, and leave the mouse and keyboard alone while both apps open and close:

   ```
   powershell -NoProfile -ExecutionPolicy Bypass -File bench\compare.ps1 -Email <e-mail> -AirAccount "<name>"
   ```

   `<name>` is what Air Explorer's address bar shows in "Meu Drive [<name>]". `-Only start,winsxs`
   runs some measurements; `-Apps airexplorer` one app. Results go to `bench\results\`.

Air Explorer keeps unfinished transfers and resumes them at its next start: if a run is cut short
while it copies, cancel what's left in its queue (select the rows, Shift+End, right click,
Cancelar) before running again, or it fills the download folder.

`uia-dump.ps1` (an app's UI Automation tree) and `list-count.ps1` (Air Explorer's list counts) are
the read-only tools the script was written with. The examples `drive_ls` (a Drive folder's
contents and sizes) and `download_speed` (one file in one stream and in segments) helped choose
the test data and how big files come down.
