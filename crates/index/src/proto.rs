//! What the app and the service say over the pipe. A request is one JSON line; the answer is a
//! JSON line ([`Header`]) and then one line per file: `d` or `f`, size, date (seconds since
//! 1970), path, separated by tabs. Windows names can't hold tabs or line breaks.

use std::io::{BufRead, Write};

use openexplorer_i18n::{t, tr};
use serde::{Deserialize, Serialize};

use crate::tree::Row;

/// The service's name, as Windows knows it.
pub const SERVICE: &str = "OpenExplorerIndex";
/// Where the service listens.
pub const PIPE: &str = r"\\.\pipe\OpenExplorerIndex";
/// The longest request read, in bytes.
pub const MAX_REQUEST: usize = 64 << 10;

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Request {
    pub text: String,
    /// A folder to search under ("C:\Users\Ana"); everywhere when `None`.
    pub within: Option<String>,
    /// The most rows to send.
    pub limit: usize,
    /// Leaves out hidden files and dotfiles, and whatever is inside hidden folders below
    /// `within`. Absent (an older app): everything, as before.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub skip_hidden: bool,
    /// Answers in English rather than Portuguese: the app's language, which the service, a
    /// process of its own, doesn't otherwise know. Absent (an older app): Portuguese.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub english: bool,
}

/// One volume's state.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Volume {
    /// "C:\".
    pub root: String,
    pub files: usize,
    /// Still being read: its results are partial or missing.
    pub indexing: bool,
    /// Why it isn't indexed, when it couldn't be.
    pub error: Option<String>,
    /// How long reading it took, in milliseconds, once done.
    pub took_ms: Option<u64>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Header {
    /// Matches in all, of which [`Header::rows`] follow.
    pub total: usize,
    pub rows: usize,
    pub volumes: Vec<Volume>,
    /// When the request itself failed.
    pub error: Option<String>,
    /// How long the search took in the service, in microseconds.
    pub took_us: u64,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Answer {
    pub header: Header,
    pub rows: Vec<Row>,
}

pub fn write_request(w: &mut impl Write, r: &Request) -> std::io::Result<()> {
    let mut line = serde_json::to_vec(r)?;
    line.push(b'\n');
    w.write_all(&line)?;
    w.flush()
}

/// Reads a request, refusing one longer than [`MAX_REQUEST`].
pub fn read_request(r: &mut impl BufRead) -> Result<Request, String> {
    let mut line = Vec::new();
    std::io::Read::take(&mut *r, MAX_REQUEST as u64 + 1).read_until(b'\n', &mut line).map_err(|e| e.to_string())?;
    if line.len() > MAX_REQUEST {
        return Err(t("Pedido grande demais.", "Request too large.").into());
    }
    serde_json::from_slice(&line).map_err(|e| tr!("Pedido inválido: {e}", "Invalid request: {e}"))
}

pub fn write_answer(w: &mut impl Write, a: &Answer) -> std::io::Result<()> {
    serde_json::to_writer(&mut *w, &a.header)?;
    w.write_all(b"\n")?;
    for r in &a.rows {
        writeln!(w, "{}\t{}\t{}\t{}", if r.is_dir { 'd' } else { 'f' }, r.size, r.modified, r.path)?;
    }
    w.flush()
}

pub fn read_answer(r: &mut impl BufRead) -> Result<Answer, String> {
    let bad = |e: &dyn std::fmt::Display| tr!("Resposta inválida do serviço: {e}", "Invalid answer from the service: {e}");
    let mut line = String::new();
    r.read_line(&mut line).map_err(|e| bad(&e))?;
    let header: Header = serde_json::from_str(&line).map_err(|e| bad(&e))?;
    let mut rows = Vec::with_capacity(header.rows);
    for _ in 0..header.rows {
        line.clear();
        if r.read_line(&mut line).map_err(|e| bad(&e))? == 0 {
            return Err(bad(&t("terminou antes da hora", "it ended too soon")));
        }
        let mut f = line.trim_end_matches(['\r', '\n']).splitn(4, '\t');
        let (Some(kind), Some(size), Some(modified), Some(path)) = (f.next(), f.next(), f.next(), f.next()) else { return Err(bad(&line)) };
        rows.push(Row { path: path.to_string(), is_dir: kind == "d", size: size.parse().map_err(|e| bad(&e))?, modified: modified.parse().map_err(|e| bad(&e))? });
    }
    Ok(Answer { header, rows })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_and_answers_round_trip() {
        let req = Request { text: "relatório 2025".into(), within: Some("C:\\Users\\Ana Maria".into()), limit: 100, skip_hidden: true, english: true };
        let mut buf = Vec::new();
        write_request(&mut buf, &req).unwrap();
        assert_eq!(read_request(&mut buf.as_slice()).unwrap(), req);
        let answer = Answer {
            header: Header { total: 9, rows: 2, volumes: vec![Volume { root: "C:\\".into(), files: 3, indexing: false, error: None, took_ms: Some(5) }], error: None, took_us: 40 },
            rows: vec![Row { path: "C:\\Users\\Ana Maria\\Relatório 2025.pdf".into(), is_dir: false, size: 1234, modified: 99 }, Row { path: "C:\\x y".into(), is_dir: true, size: 0, modified: 0 }],
        };
        let mut buf = Vec::new();
        write_answer(&mut buf, &answer).unwrap();
        assert_eq!(read_answer(&mut buf.as_slice()).unwrap(), answer);
        // Cut short, or too long, is an error rather than a hang or a huge read.
        assert!(read_answer(&mut &buf[..buf.len() - 10]).is_err());
        let huge = vec![b'x'; MAX_REQUEST + 10];
        assert!(read_request(&mut huge.as_slice()).is_err());
    }
}
