//! WebDAV, over ureq: PROPFIND lists, GET and PUT move the files, MKCOL, MOVE and DELETE do the
//! rest.

use std::io::{Read, Write};
use std::time::{Duration, SystemTime};

use base64::Engine;
use percent_encoding::{AsciiSet, CONTROLS, percent_decode_str, utf8_percent_encode};
use quick_xml::Reader;
use quick_xml::events::Event;
use ureq::Agent;
use ureq::http::{Method, Request, Response};

use crate::{CANCELLED, Conn, Item, Progress, Protocol, Result, Server, TIMEOUT, pump};

pub struct Dav {
    agent: Agent,
    /// `https://host:port`, without a path.
    origin: String,
    host: String,
    auth: Option<String>,
}

/// What a path segment may not hold unescaped.
const SEGMENT: &AsciiSet = &CONTROLS.add(b' ').add(b'"').add(b'#').add(b'%').add(b'<').add(b'>').add(b'?').add(b'`').add(b'{').add(b'}').add(b'/').add(b'[').add(b']').add(b'^').add(b'|').add(b'\\');

const PROPS: &str = r#"<?xml version="1.0" encoding="utf-8"?><d:propfind xmlns:d="DAV:"><d:prop><d:resourcetype/><d:getcontentlength/><d:getlastmodified/></d:prop></d:propfind>"#;

fn method(name: &str) -> Method {
    Method::from_bytes(name.as_bytes()).expect("a valid method name")
}

impl Dav {
    pub fn connect(server: &Server, secret: &str, trust: &crate::tls::Trust) -> Result<Dav> {
        use ureq::unversioned::resolver::DefaultResolver;
        use ureq::unversioned::transport::{Connector, TcpConnector};
        let config = Agent::config_builder()
            .http_status_as_error(false)
            .allow_non_standard_methods(true)
            .max_redirects(0)
            .timeout_connect(Some(TIMEOUT))
            .timeout_recv_response(Some(Duration::from_secs(60)))
            // The connector below goes straight to the server: no proxy from the environment.
            .proxy(None)
            .build();
        // TCP, then TLS that trusts the certificate the user did (see `tls`).
        let connector = ().chain(TcpConnector::default()).chain(crate::tls::Connector { config: crate::tls::config(trust) });
        let agent = Agent::with_parts(config, connector, DefaultResolver::default());
        let scheme = if server.protocol == Protocol::Davs { "https" } else { "http" };
        let host = if server.host.contains(':') { format!("[{}]", server.host) } else { server.host.clone() };
        let port = if server.port == server.protocol.default_port() { String::new() } else { format!(":{}", server.port) };
        let auth = (!server.user.is_empty() || !secret.is_empty()).then(|| {
            let pair = format!("{}:{secret}", server.user);
            format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(pair))
        });
        let mut dav = Dav { agent, origin: format!("{scheme}://{host}{port}"), host: server.host.clone(), auth };
        // Checks the address and the login now, with a clear message.
        dav.stat(if server.root.is_empty() { "/" } else { &server.root })?;
        Ok(dav)
    }

    /// The address of `path`, each segment escaped; a folder's ends in `/`.
    fn url(&self, path: &str, dir: bool) -> String {
        let mut out = self.origin.clone();
        for seg in path.split('/').filter(|s| !s.is_empty()) {
            out.push('/');
            out.extend(utf8_percent_encode(seg, SEGMENT));
        }
        if dir || out.len() == self.origin.len() {
            out.push('/');
        }
        out
    }

    fn request(&self, name: &str, url: &str) -> ureq::http::request::Builder {
        let mut r = Request::builder().method(method(name)).uri(url);
        if let Some(a) = &self.auth {
            r = r.header("Authorization", a);
        }
        r
    }

    /// Sends a request with a body that can be sent again: a redirect to the same server (a
    /// folder's address without its `/`) is followed once.
    fn call(&self, name: &str, url: &str, headers: &[(&str, String)], body: &str) -> Result<Response<ureq::Body>> {
        let mut url = url.to_string();
        for _ in 0..2 {
            let mut r = self.request(name, &url);
            for (k, v) in headers {
                r = r.header(*k, v);
            }
            let req = r.body(body.to_string()).map_err(|e| e.to_string())?;
            let res = self.agent.run(req).map_err(|e| self.failed(e))?;
            if matches!(res.status().as_u16(), 301 | 302 | 307 | 308)
                && let Some(to) = res.headers().get("location").and_then(|v| v.to_str().ok())
            {
                url = if to.starts_with('/') { format!("{}{to}", self.origin) } else { to.to_string() };
                if !url.strip_prefix(&self.origin).is_some_and(|rest| rest.is_empty() || rest.starts_with('/')) {
                    break;
                }
                continue;
            }
            return Ok(res);
        }
        Err(format!("{} redirecionou para outro endereço.", self.host))
    }

    fn failed(&self, e: ureq::Error) -> String {
        match e {
            ureq::Error::Io(e) => crate::unreachable(&self.host, &e),
            ureq::Error::HostNotFound => format!("Não encontrei o servidor {}.", self.host),
            ureq::Error::ConnectionFailed => format!("{} recusou a conexão: o servidor não está rodando nessa porta.", self.host),
            ureq::Error::Timeout(_) => format!("{} não respondeu a tempo.", self.host),
            ureq::Error::Tls(m) => format!("Erro de TLS com {}: {m}", self.host),
            ureq::Error::Rustls(e) => format!("Erro de TLS com {}: {e}", self.host),
            e => format!("Erro com {}: {e}", self.host),
        }
    }

    /// The message for an answer that isn't a success.
    fn refused(&self, status: u16) -> String {
        match status {
            401 => format!("{} recusou o usuário ou a senha.", self.host),
            403 => "O servidor negou a permissão.".into(),
            404 | 409 => "Não existe no servidor.".into(),
            405 => "O servidor não permite isso aqui (ou o item já existe).".into(),
            412 => "Já existe um item com esse nome.".into(),
            423 => "O item está bloqueado no servidor.".into(),
            507 => "O servidor está sem espaço.".into(),
            s => format!("O servidor respondeu {s}."),
        }
    }

    fn propfind(&self, path: &str, depth: &str, dir: bool) -> Result<Option<Vec<(String, Item)>>> {
        let res = self.call("PROPFIND", &self.url(path, dir), &[("Depth", depth.into()), ("Content-Type", "application/xml; charset=utf-8".into())], PROPS)?;
        match res.status().as_u16() {
            207 => {}
            404 => return Ok(None),
            s => return Err(self.refused(s)),
        }
        let text = res.into_body().read_to_string().map_err(|e| format!("A resposta do servidor não chegou inteira: {e}"))?;
        Ok(Some(parse_multistatus(&text)))
    }

    fn ok(&self, res: Response<ureq::Body>) -> Result<()> {
        match res.status().as_u16() {
            200..=299 => Ok(()),
            s => Err(self.refused(s)),
        }
    }
}

/// The items of a multistatus answer, each with its decoded path.
pub(crate) fn parse_multistatus(text: &str) -> Vec<(String, Item)> {
    let mut out = Vec::new();
    let mut reader = Reader::from_str(text);
    reader.config_mut().trim_text(true);
    // Inside a <response>: its href, and what its successful propstat said.
    let mut href: Option<String> = None;
    let mut item: Option<(bool, Option<u64>, Option<SystemTime>)> = None;
    // Inside a <propstat>: what it says, and whether its status is a success.
    let mut props = (false, None, None);
    let mut good = true;
    let mut text = String::new();
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_ascii_lowercase();
                match name.as_str() {
                    "response" => {
                        href = None;
                        item = None;
                    }
                    "propstat" => {
                        props = (false, None, None);
                        good = true;
                    }
                    "collection" => props.0 = true,
                    _ => {}
                }
                text.clear();
            }
            Ok(Event::Empty(e)) => {
                if e.local_name().as_ref().eq_ignore_ascii_case(b"collection") {
                    props.0 = true;
                }
            }
            Ok(Event::Text(t)) => text.push_str(&t.decode().unwrap_or_default()),
            // quick-xml hands `&amp;` and the like over apart from the text around them.
            Ok(Event::GeneralRef(r)) => {
                let name = r.decode().unwrap_or_default();
                match r.resolve_char_ref() {
                    Ok(Some(c)) => text.push(c),
                    _ => text.push_str(quick_xml::escape::resolve_xml_entity(&name).unwrap_or("")),
                }
            }
            Ok(Event::End(e)) => {
                let name = String::from_utf8_lossy(e.local_name().as_ref()).to_ascii_lowercase();
                let text = std::mem::take(&mut text);
                match name.as_str() {
                    "href" => href = Some(text),
                    "getcontentlength" => props.1 = text.trim().parse().ok(),
                    "getlastmodified" => props.2 = chrono::DateTime::parse_from_rfc2822(text.trim()).ok().map(SystemTime::from),
                    "status" => good = text.contains(" 200"),
                    "propstat" if good => item = Some(props),
                    "response" => {
                        // A member the server only gives a status for (403, 404) isn't listed.
                        if let (Some(h), Some((is_dir, size, modified))) = (href.take(), item.take()) {
                            // The href may be a whole address: keep its path.
                            let path = match h.find("://") {
                                Some(i) => h[i + 3..].find('/').map(|j| h[i + 3 + j..].to_string()).unwrap_or_else(|| "/".into()),
                                None => h,
                            };
                            let path = percent_decode_str(&path).decode_utf8_lossy().into_owned();
                            let trimmed = path.trim_end_matches('/');
                            let name = trimmed.rsplit('/').next().unwrap_or("").to_string();
                            let is_dir = is_dir || path.ends_with('/') && size.is_none();
                            out.push((trimmed.to_string(), Item { name, is_dir, size: if is_dir { None } else { size.or(Some(0)) }, modified, link: false }));
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
    }
    out
}

/// Whether two server paths name the same place, whatever their final `/`.
fn same(a: &str, b: &str) -> bool {
    a.trim_end_matches('/') == b.trim_end_matches('/')
}

impl Conn for Dav {
    fn list(&mut self, dir: &str) -> Result<Vec<Item>> {
        let all = self.propfind(dir, "1", true)?.ok_or_else(|| "Não existe no servidor.".to_string())?;
        // The answer starts with the folder itself.
        Ok(all.into_iter().filter(|(p, _)| !same(p, dir) && !p.is_empty()).map(|(_, i)| i).collect())
    }

    fn stat(&mut self, path: &str) -> Result<Option<Item>> {
        Ok(self.propfind(path, "0", false)?.and_then(|v| v.into_iter().next()).map(|(_, mut i)| {
            if i.name.is_empty() {
                i.name = "/".into();
            }
            i
        }))
    }

    /// WebDAV has no links.
    fn is_link(&mut self, _: &str) -> Result<bool> {
        Ok(false)
    }

    fn mkdir(&mut self, path: &str) -> Result<()> {
        let res = self.call("MKCOL", &self.url(path, true), &[], "")?;
        if res.status().as_u16() == 405 {
            return Err("Já existe um item com esse nome.".into());
        }
        self.ok(res)
    }

    fn rename(&mut self, from: &str, to: &str) -> Result<()> {
        let dir = self.stat(from)?.is_some_and(|i| i.is_dir);
        let res = self.call("MOVE", &self.url(from, dir), &[("Destination", self.url(to, dir)), ("Overwrite", "F".into())], "")?;
        self.ok(res)
    }

    /// COPY, with all a folder holds (Depth infinity), never over something there.
    fn copy(&mut self, from: &str, to: &str) -> Result<bool> {
        let dir = self.stat(from)?.is_some_and(|i| i.is_dir);
        let res = self.call("COPY", &self.url(from, dir), &[("Destination", self.url(to, dir)), ("Overwrite", "F".into()), ("Depth", "infinity".into())], "")?;
        self.ok(res).map(|_| true)
    }

    fn remove_file(&mut self, path: &str) -> Result<()> {
        let res = self.call("DELETE", &self.url(path, false), &[], "")?;
        self.ok(res)
    }

    fn remove_dir(&mut self, path: &str) -> Result<()> {
        let res = self.call("DELETE", &self.url(path, true), &[], "")?;
        self.ok(res)
    }

    /// DELETE takes a folder and all it holds at once.
    fn remove(&mut self, path: &str, is_dir: bool) -> Result<()> {
        if is_dir { self.remove_dir(path) } else { self.remove_file(path) }
    }

    fn download(&mut self, path: &str, out: &mut dyn Write, progress: Progress) -> Result<()> {
        let res = self.call("GET", &self.url(path, false), &[], "")?;
        if !res.status().is_success() {
            return Err(self.refused(res.status().as_u16()));
        }
        let mut body = res.into_body();
        let mut reader = body.as_reader();
        pump(&mut reader, out, progress).map(|_| ())
    }

    fn upload(&mut self, path: &str, src: &mut dyn Read, len: u64, progress: Progress) -> Result<()> {
        // Counts what was read off the file, which is what has gone out.
        struct Counting<'a, 'b> {
            src: &'a mut dyn Read,
            done: u64,
            progress: Progress<'b>,
            stopped: bool,
        }
        impl Read for Counting<'_, '_> {
            fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
                let n = self.src.read(buf)?;
                self.done += n as u64;
                if !(self.progress)(self.done) {
                    self.stopped = true;
                    return Err(std::io::Error::other(CANCELLED));
                }
                Ok(n)
            }
        }
        let mut counting = Counting { src, done: 0, progress, stopped: false };
        // A length, not chunks: some servers (nginx) refuse a chunked upload.
        let req = self
            .request("PUT", &self.url(path, false))
            .header("Content-Length", len.to_string())
            .body(ureq::SendBody::from_reader(&mut counting))
            .map_err(|e| e.to_string())?;
        let res = self.agent.run(req);
        if counting.stopped {
            // The server may still be writing what arrived, and keep it once the request ends:
            // delete until it's gone.
            for _ in 0..10 {
                let _ = self.remove_file(path);
                if matches!(self.stat(path), Ok(None)) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
            return Err(CANCELLED.into());
        }
        self.ok(res.map_err(|e| self.failed(e))?)
    }

    fn home(&mut self) -> Result<String> {
        Ok("/".into())
    }

    fn alive(&mut self) -> bool {
        // Each request is its own; ureq keeps the connection open itself.
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_multistatus_from_apache_and_nextcloud() {
        let apache = r#"<?xml version="1.0" encoding="utf-8"?>
<D:multistatus xmlns:D="DAV:" xmlns:ns0="DAV:">
<D:response xmlns:lp1="DAV:"><D:href>/dav/Fotos%20de%20f%C3%A9rias/</D:href>
<D:propstat><D:prop><lp1:resourcetype><D:collection/></lp1:resourcetype><lp1:getlastmodified>Mon, 12 Feb 2024 10:00:00 GMT</lp1:getlastmodified></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
</D:response>
<D:response><D:href>/dav/Fotos%20de%20f%C3%A9rias/a&amp;b.jpg</D:href>
<D:propstat><D:prop><lp1:resourcetype xmlns:lp1="DAV:"/><lp1:getcontentlength xmlns:lp1="DAV:">1234</lp1:getcontentlength></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat>
<D:propstat><D:prop><D:getlastmodified/></D:prop><D:status>HTTP/1.1 404 Not Found</D:status></D:propstat>
</D:response>
</D:multistatus>"#;
        let items = parse_multistatus(apache);
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].0, "/dav/Fotos de férias");
        assert!(items[0].1.is_dir);
        assert!(items[0].1.modified.is_some());
        assert_eq!(items[1].1, Item { name: "a&b.jpg".into(), is_dir: false, size: Some(1234), modified: None, link: false });

        // A member given only a status (no access) isn't listed.
        let nextcloud = r#"<?xml version="1.0"?><d:multistatus xmlns:d="DAV:" xmlns:s="http://sabredav.org/ns"><d:response><d:href>https://cloud.example.com/remote.php/dav/files/ana/Docs/</d:href><d:propstat><d:prop><d:resourcetype><d:collection/></d:resourcetype></d:prop><d:status>HTTP/1.1 200 OK</d:status></d:propstat></d:response><d:response><d:href>/remote.php/dav/files/ana/Docs/segredo</d:href><d:status>HTTP/1.1 403 Forbidden</d:status></d:response></d:multistatus>"#;
        let items = parse_multistatus(nextcloud);
        assert_eq!(items.len(), 1, "{items:?}");
        assert_eq!(items[0].0, "/remote.php/dav/files/ana/Docs");
        assert_eq!(items[0].1.name, "Docs");
        assert!(items[0].1.is_dir);
    }

    #[test]
    fn addresses_escape_each_segment() {
        let server = Server { id: "x".into(), name: "x".into(), protocol: Protocol::Davs, host: "h".into(), port: 443, user: String::new(), root: "/".into(), key: None, host_key: None, cert: None, connections: 4, implicit: false };
        let dav = Dav { agent: Agent::new_with_defaults(), origin: "https://h".into(), host: server.host.clone(), auth: None };
        assert_eq!(dav.url("/a b/c#d%", false), "https://h/a%20b/c%23d%25");
        assert_eq!(dav.url("/a", true), "https://h/a/");
        assert_eq!(dav.url("/", true), "https://h/");
        assert_eq!(dav.url("", false), "https://h/");
    }
}
