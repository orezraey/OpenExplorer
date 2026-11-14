//! The transfer queue under the panes, as in Air Explorer: every copy, move, upload and
//! download is a row per item, with where it comes from and goes to, its
//! state, size, progress, speed and time left. A job works through its rows in turn; Cancelar
//! stops one row or all of them.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

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
            Op::Copy => "Copiar",
            Op::Move => "Mover",
            Op::Upload => "Upload",
            Op::Download => "Download",
        }
    }

    /// "Copiado", "Movido": for the line that sums a job up.
    fn done(self) -> &'static str {
        match self {
            Op::Copy => "Copiado",
            Op::Move => "Movido",
            Op::Upload => "Enviado",
            Op::Download => "Baixado",
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
        parts.extend(name.map(str::to_string));
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
}

impl Transfer {
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

/// The rows one job works through. Rows it never got to (the job panicked, or a Drive account
/// wouldn't open) end as failed when it's dropped, so none stays "Em andamento" forever.
pub struct Batch {
    op: Op,
    rows: Vec<Arc<Progress>>,
}

impl Drop for Batch {
    fn drop(&mut self) {
        for p in self.rows.iter().filter(|p| !p.over()) {
            p.end(Err("Interrompido.".into()));
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
            p.end(each(i, p));
        }
        self.summary()
    }

    /// How the rows ended, in a line: "Copiado: 3 itens.", or what went wrong.
    pub fn summary(&self) -> Result<String, String> {
        let count = |stage| self.rows.iter().filter(|p| p.stage() == stage).count();
        let (ok, cancelled) = (count(Stage::Done), count(Stage::Cancelled));
        let failed: Vec<String> = self.rows.iter().filter(|p| p.stage() == Stage::Failed).map(|p| p.error()).collect();
        match (ok, cancelled, failed.as_slice()) {
            (0, 0, [why]) => Err(why.clone()),
            (_, 0, []) => Ok(format!("{}: {}.", self.op.done(), items(ok))),
            (_, _, []) => Ok(format!("{}: {} · cancelados: {cancelled}.", self.op.done(), items(ok))),
            (_, _, [first, ..]) => Err(format!("{}: {} · com erro: {} ({first})", self.op.done(), items(ok), failed.len())),
        }
    }
}

fn items(n: usize) -> String {
    if n == 1 { "1 item".into() } else { format!("{n} itens") }
}

/// Adds a row per `(from, to)` to `list`, waiting their turn; the job doing them holds the
/// returned [`Batch`].
pub fn queue(list: &mut Vec<Transfer>, wake: Option<rune::Waker>, op: Op, rows: impl IntoIterator<Item = (End, End)>) -> Batch {
    let mut batch = Batch { op, rows: Vec::new() };
    for (from, to) in rows {
        let progress = Arc::new(Progress::new(wake.clone()));
        batch.rows.push(progress.clone());
        list.push(Transfer { op, from, to, progress });
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
    let f = |b: u64| if u == 0 { b.to_string() } else { format!("{:.2}", b as f64 / scale).replace('.', ",") };
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

    #[test]
    fn rows_a_job_never_reached_end_as_failed() {
        let mut list = Vec::new();
        drop(queue(&mut list, None, Op::Move, [(End::local(Path::new("a")), End::local(Path::new("b")))]));
        assert_eq!(list[0].progress.stage(), Stage::Failed);
    }
}
