# Testing and benchmarking a provider

## Tests in the crate

- **The fake** (`fake.rs`) serves the API from memory on a local port. It does what the real one
  does and nothing friendlier:
  - `Range`, answering 206 to every ranged request, including a cut one;
  - 429 with `Retry-After`;
  - dropped connections mid-body;
  - batch endpoints;
  - both list orders;
  - request counters, so tests can assert how many calls something costs.
- **One test per behaviour, named as a sentence**: `a_big_file_comes_in_parallel_segments_that_each_resume`,
  `a_count_from_before_doesnt_cut_a_big_folder_short`.
- **A bug's test must fail without the fix.** Check it once: put the old line back, run the test,
  restore.
- **Timing tests**: assert a generous bound, not an exact time. Tunables like stall time or split
  size are passed in, so tests run in milliseconds (`Split { … }` in `gdrive/src/download.rs`).
- **Watch for flaky orderings.** Threads finish in any order, and two consecutive
  `Instant::now()` can be equal (sleep 2 ms where order matters).
- **Real-account tests** go in `tests/real_account.rs` with `#[ignore]`, and clean up what they
  make.

## Live checks without the GUI

`examples/<provider>_bench.rs <account>` with no subcommand fills `Open Explorer benchmark` with:
- `1000 folders`;
- `5000 files` of 1 KB;
- `100 folders`;
- `transfer/large/large.bin` (500 MiB of non-repeating bytes).

Subcommands, used by the harness:

| Subcommand | What it does |
|---|---|
| `make <n>` | makes upload folders under `uploads`, one name per line |
| `wait <folder> <files> <bytes>` | prints `ready`, polls a fresh listing every 200 ms, prints `done` |
| `clean` | deletes the upload folders |
| `roundtrip` | sends files, reads them back so the provider's check runs, times both, deletes them |

`roundtrip` exists so far only in `mega_bench` and `dropbox_bench`. Add it to a new provider's
example, and to `onedrive_bench` and Drive's `upload_target` when their uploads change.

For quick A/B comparisons there are speed examples like `gdrive/examples/download_speed.rs`.
Alternate the old and new binaries, 15 to 20 runs each, and compare median and worst.

## Against Air Explorer: `bench/compare.ps1`

1. Add the provider to `$Clouds`: `Oe` is the start of Open Explorer's address for the account,
   and `Air` is the start of Air Explorer's address bar for it (`'Dropbox ['`), which picks its
   tab. Add it to `$Waiters` too (its example and account key). Its transfers then get named
   `<provider>-download-large` and so on.
2. Connect the same account in both apps; Air Explorer's free version allows one per cloud.
   Build the app with the app closed, and build the examples.
3. Run a battery:

   ```
   powershell -NoProfile -ExecutionPolicy Bypass -File bench\compare.ps1 -Apps "openexplorer,airexplorer" -Only <list> -Runs 2
   ```

   Then rerun the close rows in alternating pairs (`-Apps airexplorer`, then `openexplorer`, a
   few times). The line's speed drifts from minute to minute.
4. Any row where Open Explorer is slower is a bug: find the cause (log timings, a headless A/B),
   fix it, measure again.

Harness behaviour to know:
- It drives both apps on screen, so the user must not touch the mouse.
- A failed run is reported and the next measurement goes on.
- Air Explorer keeps transfers queued after a failed run, and they slow the next ones. Check its
  window, `%APPDATA%\AirExplorer\tasks.xml`, and stray `C:\OEBench` files.

## Commit

Put a small table in the commit body:

- what was measured and how;
- before and after;
- Air Explorer's number;
- the command to rerun it.

If something meant to be faster wasn't measured, say so.
