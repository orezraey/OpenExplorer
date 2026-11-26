//! The transfer queue under the panes, as in Air Explorer: every copy, move, upload and
//! download is a row per item, with where it comes from and goes to, its
//! state, size, progress, speed and time left. A job works through its rows in turn; Cancelar
//! stops one row or all of them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use openexplorer_i18n::{t, tr};

use crate::model::{Location, crumbs};
use crate::ops::{CANCELLED, Progress, Stage};

/// What a row does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Copy,
    Move,
    /// From the computer to Google Drive.
    Upload,
    /// From Google Drive to the computer.
    Download,
}

impl Op {
    pub fn label(self) -> &'static str {
        match self {
            Op::Copy => t("Copiar", "Copy"),
            Op::Move => t("Mover", "Move"),
            Op::Upload => t("Upload", "Upload"),
            Op::Download => t("Download", "Download"),
        }
    }

    /// "Copiado", "Movido": for the line that sums a job up.
    fn done(self) -> &'static str {
        match self {
            Op::Copy => t("Copiado", "Copied"),
            Op::Move => t("Movido", "Moved"),
            Op::Upload => t("Enviado", "Uploaded"),
            Op::Download => t("Baixado", "Downloaded"),
        }
    }
}

/// One end of a row, for its Origem or Destino column.
#[derive(Clone, Debug, PartialEq)]
pub struct End {
    /// The full path, or "Meu Drive\Pasta [conta]".
    pub text: String,
    /// The local disk it's on, for that disk's icon; `None` on Google Drive.
    pub disk: Option<PathBuf>,
}

impl End {
    pub fn local(path: &Path) -> End {
        let disk = path.ancestors().last().map(Path::to_path_buf);
        End { text: path.display().to_string(), disk }
    }

    /// Item `name` in a Drive folder (`None` for the folder itself).
    pub fn drive(loc: &Location, name: Option<&str>) -> End {
        let mut parts: Vec<String> = crumbs(loc).into_iter().skip(1).map(|c| c.label).collect();
        parts.extend(name.map(crate::model::shown));
        let account = match loc {
            Location::Drive { email, .. } => format!(" [{email}]"),
            _ => String::new(),
        };
        End { text: format!("{}{account}", parts.join("\\")), disk: None }
    }
}

/// A row of the queue.
pub struct Transfer {
    pub op: Op,
    pub from: End,
    pub to: End,
    pub progress: Arc<Progress>,
    /// What its row shows that moves while it runs.
    pub shown: Shown,
}

/// The progress bar's height, and its widest.
pub const BAR_H: f32 = 15.0;
pub const BAR_MAX: f32 = 120.0;

/// The bar's width in a Progresso column `col` wide: its cell keeps 6 points each side.
pub fn bar_width(col: f32) -> f32 {
    (col - 12.0).clamp(0.0, BAR_MAX)
}

/// What a row shows that changes as it runs, bound to the window as live values: `poll`
/// sets them ([`Transfer::refresh`]) and the window redraws them without building the view
/// again. Measured with 40 downloads running (`queue_progress_cost`).
#[derive(Clone)]
pub struct Shown {
    /// "Em andamento · 120 de 1.946", Telegram's wait, or why it failed.
    pub state: rune::LiveText,
    pub size: rune::LiveText,
    pub percent: rune::LiveText,
    /// 0 to 1, for assistive technology.
    pub fraction: rune::Live<f32>,
    /// The bar's fill: its rect inside the bar, past its 1-point border.
    pub fill: rune::Live<[f32; 4]>,
    pub speed: rune::LiveText,
    pub left: rune::LiveText,
    /// The look and bar width (its bits) it was last set for: a row over is set again only
    /// when one of them changes.
    seen: std::cell::Cell<Option<(Look, u32)>>,
}

impl Default for Shown {
    fn default() -> Shown {
        Shown {
            state: rune::LiveText::new(""),
            size: rune::LiveText::new(""),
            percent: rune::LiveText::new(""),
            fraction: rune::Live::new(0.0),
            fill: rune::Live::new([1.0, 1.0, 0.0, BAR_H - 2.0]),
            speed: rune::LiveText::new(""),
            left: rune::LiveText::new(""),
            seen: std::cell::Cell::new(None),
        }
    }
}

/// What only building the row again shows: its stage's dot and colours, and a failure's
/// message on hover. While it stays the same, the row's live values are enough.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Look {
    stage: Stage,
    cancelled: bool,
}

impl Transfer {
    pub fn look(&self) -> Look {
        Look { stage: self.progress.stage(), cancelled: self.progress.cancelled() }
    }

    /// The Estado column's words: the stage, a folder's count while running, Telegram's wait,
    /// or why it failed.
    pub fn state_text(&self) -> String {
        let p = &self.progress;
        let stage = match p.stage() {
            Stage::Waiting | Stage::Running if p.cancelled() => t("Cancelando…", "Canceling…"),
            Stage::Waiting => t("Aguardando", "Waiting"),
            Stage::Running => t("Em andamento", "In progress"),
            Stage::Done => t("Concluído", "Done"),
            Stage::Cancelled => t("Cancelado", "Canceled"),
            Stage::Failed => t("Erro", "Error"),
            Stage::Skipped => t("Ignorado", "Skipped"),
        };
        let items = p.items.load(Ordering::Relaxed);
        if p.stage() == Stage::Running {
            // Telegram asked to wait: the row says for how long, rather than look stuck.
            if let Some(note) = crate::telegram::waiting_note(&self.from.text, &self.to.text) {
                return note;
            }
            // A folder's row counts its items: "Em andamento · 120 de 1.946".
            if items > 1 {
                let (done, items) = (crate::ui::thousands(p.items_done.load(Ordering::Relaxed) as usize), crate::ui::thousands(items as usize));
                return tr!("{stage} · {done} de {items}", "{stage} · {done} of {items}");
            }
        }
        if p.stage() == Stage::Failed && !p.reason().is_empty() {
            // The reason itself, not just "Erro": the whole message stays on hover.
            return p.reason();
        }
        stage.to_string()
    }

    /// Sets the row's live values from where it stands, for a bar `bar_w` wide.
    pub fn refresh(&self, bar_w: f32) {
        let p = &self.progress;
        let s = &self.shown;
        s.seen.set(Some((self.look(), bar_w.to_bits())));
        s.state.set(self.state_text());
        if p.stage() == Stage::Skipped {
            s.size.set(String::new());
            s.percent.set("—");
            s.fraction.set(0.0);
            s.fill.set([1.0, 1.0, 0.0, BAR_H - 2.0]);
            s.speed.set(String::new());
            s.left.set(String::new());
            return;
        }
        s.size.set(if p.sized.load(Ordering::Relaxed) { pair(p.bytes.load(Ordering::Relaxed), p.total.load(Ordering::Relaxed)) } else { String::new() });
        let fraction = self.fraction();
        s.percent.set(fraction.map(|f| format!("{}%", (f * 100.0).floor())).unwrap_or_else(|| "…".into()));
        s.fraction.set(fraction.unwrap_or(0.0));
        s.fill.set([1.0, 1.0, (bar_w - 2.0).max(0.0) * fraction.unwrap_or(0.0), BAR_H - 2.0]);
        s.speed.set(self.speed().map(|v| format!("{}/s", crate::model::format_size(v as u64))).unwrap_or_default());
        s.left.set(self.left().map(hms).unwrap_or_default());
    }
    /// How far along, 0 to 1, or `None` while running with no size to measure by (a Drive
    /// folder, copied on Google's side).
    pub fn fraction(&self) -> Option<f32> {
        let p = &self.progress;
        let total = p.total.load(Ordering::Relaxed);
        let sized = p.sized.load(Ordering::Relaxed) && total > 0;
        let measured = || (p.bytes.load(Ordering::Relaxed) as f64 / total as f64).min(1.0) as f32;
        match p.stage() {
            Stage::Done => Some(1.0),
            Stage::Running => sized.then(measured),
            // Waiting, or stopped where it got to.
            _ => Some(if sized { measured() } else { 0.0 }),
        }
    }

    /// Bytes per second while running, once a moment has gone by.
    pub fn speed(&self) -> Option<f64> {
        let p = &self.progress;
        let secs = p.running_for().filter(|s| *s >= 0.5 && p.stage() == Stage::Running)?;
        let bytes = p.bytes.load(Ordering::Relaxed);
        (bytes > 0).then(|| bytes as f64 / secs)
    }

    /// Seconds left at the current speed.
    pub fn left(&self) -> Option<f64> {
        let p = &self.progress;
        let rest = p.total.load(Ordering::Relaxed).saturating_sub(p.bytes.load(Ordering::Relaxed));
        p.sized.load(Ordering::Relaxed).then_some(())?;
        self.speed().map(|v| rest as f64 / v)
    }
}

/// The queue's foot while something is under way, as live values: "Total: 0,05 / 2,66 GB",
/// its bar, and the speed of everything running.
#[derive(Clone)]
pub struct Totals {
    pub text: rune::LiveText,
    pub fill: rune::Live<[f32; 4]>,
    pub speed: rune::LiveText,
}

/// The foot's bar: its width and height.
pub const TOTAL_BAR: (f32, f32) = (80.0, 10.0);

impl Default for Totals {
    fn default() -> Totals {
        Totals { text: rune::LiveText::new(""), fill: rune::Live::new([1.0, 1.0, 0.0, TOTAL_BAR.1 - 2.0]), speed: rune::LiveText::new("") }
    }
}

/// What changes in the queue's build: rows coming and going, and each row's [`Look`].
pub fn looks(list: &[Transfer]) -> Vec<Look> {
    list.iter().map(Transfer::look).collect()
}

/// Sets the queue's live values: each row still under way, or whose look or bar width
/// changed since it was last set (to show where it ended, or fill a column made wider), and
/// the foot.
pub fn refresh(list: &[Transfer], totals: &Totals, bar_w: f32) {
    let (mut done, mut total, mut speed) = (0u64, 0u64, 0.0);
    for t in list {
        let over = t.progress.over();
        if !over || t.shown.seen.get() != Some((t.look(), bar_w.to_bits())) {
            t.refresh(bar_w);
        }
        if !over {
            done += t.progress.bytes.load(Ordering::Relaxed);
            total += t.progress.total.load(Ordering::Relaxed);
            speed += t.speed().unwrap_or(0.0);
        }
    }
    let fraction = if total > 0 { (done as f64 / total as f64).min(1.0) as f32 } else { 0.0 };
    totals.text.set(tr!("Total: {}", "Total: {}", pair(done, total)));
    totals.fill.set([1.0, 1.0, (TOTAL_BAR.0 - 2.0) * fraction, TOTAL_BAR.1 - 2.0]);
    totals.speed.set(if speed > 0.0 { format!("{}/s", crate::model::format_size(speed as u64)) } else { String::new() });
}

/// The rows one job works through. Rows it never got to (the job panicked, or a Drive account
/// wouldn't open) end as failed when it's dropped, so none stays "Em andamento" forever.
pub struct Batch {
    op: Op,
    rows: Vec<Arc<Progress>>,
}

impl Drop for Batch {
    fn drop(&mut self) {
        for p in self.rows.iter().filter(|p| !p.over()) {
            p.end(Err(t("Interrompido.", "Interrupted.").into()));
        }
    }
}

impl Batch {
    pub fn rows(&self) -> &[Arc<Progress>] {
        &self.rows
    }

    /// Runs `each` on every row in turn, skipping cancelled ones and going on past a failure,
    /// and sums it up: "Copiado: 3 itens.", or what went wrong.
    pub fn run(&self, mut each: impl FnMut(usize, &Progress) -> Result<(), String>) -> Result<String, String> {
        for (i, p) in self.rows.iter().enumerate() {
            if p.cancelled() || !p.begin() {
                p.end(Err(CANCELLED.into()));
                continue;
            }
            let result = each(i, p);
            if p.stage() != Stage::Skipped {
                p.end(result);
            }
        }
        self.summary()
    }

    /// For jobs that run several rows at once: the next row, by the counter `at` they share,
    /// begun; cancelled ones are ended and passed over. The caller ends it with its result.
    pub fn next(&self, at: &std::sync::atomic::AtomicUsize) -> Option<(usize, &Progress)> {
        loop {
            let i = at.fetch_add(1, Ordering::Relaxed);
            let p = self.rows.get(i)?;
            if p.cancelled() || !p.begin() {
                p.end(Err(CANCELLED.into()));
                continue;
            }
            return Some((i, p));
        }
    }

    /// Ends the rows nothing ran (every worker failed to start) with `why`.
    pub fn abandon(&self, why: &str) {
        for p in self.rows.iter().filter(|p| !p.over()) {
            p.end(Err(why.to_string()));
        }
    }

    /// How the rows ended, in a line: "Copiado: 3 itens.", or what went wrong.
    pub fn summary(&self) -> Result<String, String> {
        let count = |stage| self.rows.iter().filter(|p| p.stage() == stage).count();
        let (ok, skipped, cancelled) = (count(Stage::Done), count(Stage::Skipped), count(Stage::Cancelled));
        let failed: Vec<String> = self.rows.iter().filter(|p| p.stage() == Stage::Failed).map(|p| p.error()).collect();
        if skipped > 0 && ok == 0 && cancelled == 0 && failed.is_empty() {
            return Ok(tr!("Ignorados (já existiam): {}.", "Skipped (already exist): {}.", items(skipped)));
        }
        let skip_part = if skipped > 0 {
            tr!(" · ignorados: {skipped}", " · skipped: {skipped}")
        } else {
            String::new()
        };
        match (ok, cancelled, failed.as_slice()) {
            (0, 0, [why]) => Err(why.clone()),
            (_, 0, []) => Ok(format!("{}: {}{skip_part}.", self.op.done(), items(ok))),
            (_, _, []) => Ok(tr!("{0}: {1} · cancelados: {cancelled}{skip_part}.", "{0}: {1} · cancelled: {cancelled}{skip_part}.", self.op.done(), items(ok))),
            (_, _, [first, ..]) => Err(tr!("{0}: {1} · com erro: {2} ({first}){skip_part}", "{0}: {1} · failed: {2} ({first}){skip_part}", self.op.done(), items(ok), failed.len())),
        }
    }
}

fn items(n: usize) -> String {
    if n == 1 { "1 item".into() } else { tr!("{n} itens", "{n} items") }
}

/// Adds a row per `(from, to)` to `list`, waiting their turn; the job doing them holds the
/// returned [`Batch`].
pub fn queue(list: &mut Vec<Transfer>, wake: Option<rune::Waker>, op: Op, rows: impl IntoIterator<Item = (End, End)>) -> Batch {
    let mut batch = Batch { op, rows: Vec::new() };
    for (from, to) in rows {
        let progress = Arc::new(Progress::new(wake.clone()));
        batch.rows.push(progress.clone());
        list.push(Transfer { op, from, to, progress, shown: Shown::default() });
    }
    batch
}

/// "0,05 / 2,66 GB": the second in its own unit, the first in the same one.
pub fn pair(done: u64, total: u64) -> String {
    const UNITS: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut u = 0;
    let mut scale = 1.0;
    while total as f64 / scale >= 1024.0 && u < UNITS.len() - 1 {
        scale *= 1024.0;
        u += 1;
    }
    let f = |b: u64| if u == 0 { b.to_string() } else { openexplorer_i18n::decimal(format!("{:.2}", b as f64 / scale)) };
    format!("{} / {} {}", f(done), f(total), UNITS[u])
}

/// "0h 1m 6s", as Air Explorer writes it.
pub fn hms(secs: f64) -> String {
    let s = secs.ceil() as u64;
    format!("{}h {}m {}s", s / 3600, s % 3600 / 60, s % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_times_read_as_in_air_explorer() {
        assert_eq!(pair(54_760_000, 2_858_000_000), "0,05 / 2,66 GB");
        assert_eq!(pair(10, 900), "10 / 900 B");
        assert_eq!(pair(0, 4096), "0,00 / 4,00 KB");
        assert_eq!(hms(66.0), "0h 1m 6s");
        assert_eq!(hms(3725.2), "1h 2m 6s");
    }

    #[test]
    fn a_batch_goes_on_past_failures_and_cancels() {
        let mut list = Vec::new();
        let end = || End::local(Path::new(r"C:\x"));
        let batch = queue(&mut list, None, Op::Copy, (0..4).map(|_| (end(), end())));
        // A row cancelled while waiting reads Cancelado at once, not when its turn comes.
        list[1].progress.cancel();
        assert_eq!(list[1].progress.stage(), Stage::Cancelled);
        let result = batch.run(|i, _| if i == 2 { Err("disco cheio".into()) } else { Ok(()) });
        assert_eq!(result, Err("Copiado: 2 itens · com erro: 1 (disco cheio)".into()));
        let stages: Vec<Stage> = list.iter().map(|t| t.progress.stage()).collect();
        assert_eq!(stages, [Stage::Done, Stage::Cancelled, Stage::Failed, Stage::Done]);
        assert_eq!(list[2].progress.error(), "disco cheio");
        assert_eq!(end().disk, Some(PathBuf::from(r"C:\")));
    }

    /// A running row's live values follow it without a rebuild: words, size, percent and the
    /// bar's fill; the foot sums what is under way; its look changes only with its stage.
    #[test]
    fn live_values_follow_a_running_row() {
        let mut list = Vec::new();
        let batch = queue(&mut list, None, Op::Download, [(End::local(Path::new("a")), End::local(Path::new("b")))]);
        let before = looks(&list);
        let p = list[0].progress.clone();
        p.size(1000);
        p.begin();
        p.add_bytes(250);
        let totals = Totals::default();
        refresh(&list, &totals, 102.0);
        let s = &list[0].shown;
        assert_eq!(&*s.state.get(), "Em andamento");
        assert_eq!(&*s.size.get(), "250 / 1000 B");
        assert_eq!(&*s.percent.get(), "25%");
        assert_eq!(s.fill.get(), [1.0, 1.0, 25.0, BAR_H - 2.0]);
        assert_eq!(&*totals.text.get(), "Total: 250 / 1000 B");
        assert_ne!(looks(&list), before);
        // Over, it is set once more, and again only when its column changes width.
        p.end(Ok(()));
        refresh(&list, &totals, 102.0);
        assert_eq!((&*s.state.get(), &*s.percent.get()), ("Concluído", "100%"));
        assert_eq!(s.fill.get()[2], 100.0);
        refresh(&list, &totals, 52.0);
        assert_eq!(s.fill.get()[2], 50.0);
        assert_eq!(bar_width(112.0), 100.0);
        assert_eq!(bar_width(400.0), BAR_MAX);
        drop(batch);
    }

    #[test]
    fn rows_a_job_never_reached_end_as_failed() {
        let mut list = Vec::new();
        drop(queue(&mut list, None, Op::Move, [(End::local(Path::new("a")), End::local(Path::new("b")))]));
        assert_eq!(list[0].progress.stage(), Stage::Failed);
    }

    #[test]
    fn skipped_rows_refresh_cleanly_and_summarize() {
        let mut list = Vec::new();
        let end = || End::local(Path::new(r"C:\x"));
        let batch = queue(&mut list, None, Op::Copy, (0..2).map(|_| (end(), end())));
        let summary = batch.run(|i, p| {
            if i == 0 {
                p.skip();
            }
            Ok(())
        });
        assert_eq!(summary, Ok("Copiado: 1 item · ignorados: 1.".into()));
        let totals = Totals::default();
        refresh(&list, &totals, 100.0);
        assert_eq!(&*list[0].shown.state.get(), "Ignorado");
        assert_eq!(&*list[0].shown.percent.get(), "—");

        let mut all_skipped = Vec::new();
        let b2 = queue(&mut all_skipped, None, Op::Download, [(end(), end())]);
        let s2 = b2.run(|_, p| {
            p.skip();
            Ok(())
        });
        assert_eq!(s2, Ok("Ignorados (já existiam): 1 item.".into()));
    }
}
