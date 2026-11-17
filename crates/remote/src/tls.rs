//! TLS for FTPS and WebDAV over https: a certificate the usual authorities vouch for is
//! accepted, as a browser would; one they don't (a NAS's own, self-signed, or issued for
//! another name) only when the user trusted it by its fingerprint, which is kept with the
//! server. A rejected certificate's fingerprint is noted, so adding the server can show it
//! and ask.
//!
//! WebDAV goes through ureq, which takes no verifier of its own: its connections are made by
//! [`Connector`] here, TCP then this TLS.

use std::fmt;
use std::io::{Read, Write};
use std::sync::{Arc, Mutex};

use rustls::client::WebPkiServerVerifier;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, WebPkiSupportedAlgorithms};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme, StreamOwned};
use sha2::{Digest, Sha256};
use ureq::unversioned::transport::{Buffers, ConnectionDetails, Either, LazyBuffers, NextTimeout, Transport, TransportAdapter};

/// A certificate the verifier turned down.
#[derive(Clone, Debug, PartialEq)]
pub struct Rejected {
    pub fingerprint: String,
    /// Why, in words.
    pub why: String,
}

/// What a connection trusts beyond the usual authorities, and what it turned down.
#[derive(Clone, Default)]
pub(crate) struct Trust {
    /// The fingerprint of the one certificate the user trusted for this server.
    pub pinned: Option<String>,
    pub seen: Arc<Mutex<Option<Rejected>>>,
}

impl Trust {
    pub fn new(pinned: Option<String>) -> Trust {
        Trust { pinned, seen: Arc::default() }
    }

    pub fn rejected(&self) -> Option<Rejected> {
        self.seen.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// The message for a connection to `host` that failed on its certificate, if it did.
    pub fn refusal(&self, host: &str) -> Option<String> {
        let r = self.rejected()?;
        Some(match &self.pinned {
            Some(old) => format!(
                "O certificado de {host} mudou: o confiado era {old}, o de agora é {} ({}). Se a mudança é esperada, edite o servidor para confiar no novo.",
                r.fingerprint, r.why
            ),
            None => format!("O certificado de {host} não é confiável: {}. Edite o servidor para ver a impressão digital e confiar nele.", r.why),
        })
    }
}

/// `AB:CD:…`, the SHA-256 of the certificate as browsers show it.
pub fn fingerprint(der: &[u8]) -> String {
    Sha256::digest(der).iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

fn why(e: &rustls::Error) -> String {
    match e {
        rustls::Error::InvalidCertificate(c) => match c {
            CertificateError::UnknownIssuer | CertificateError::BadSignature => "não foi emitido por uma autoridade conhecida (é um certificado próprio)".into(),
            CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. } => "foi emitido para outro nome".into(),
            CertificateError::Expired | CertificateError::ExpiredContext { .. } => "está vencido".into(),
            CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. } => "ainda não vale (o relógio está certo?)".into(),
            CertificateError::Revoked => "foi revogado".into(),
            // A NAS's own certificate is often made as a CA and used for itself.
            CertificateError::Other(o) if format!("{o:?}").contains("CaUsedAsEndEntity") => "não foi emitido por uma autoridade conhecida (é um certificado próprio)".into(),
            other => format!("é inválido ({other:?})"),
        },
        other => format!("é inválido ({other})"),
    }
}

#[derive(Debug)]
struct Verifier {
    usual: Arc<WebPkiServerVerifier>,
    algorithms: WebPkiSupportedAlgorithms,
    pinned: Option<String>,
    seen: Arc<Mutex<Option<Rejected>>>,
}

impl ServerCertVerifier for Verifier {
    fn verify_server_cert(&self, leaf: &CertificateDer<'_>, intermediates: &[CertificateDer<'_>], name: &ServerName<'_>, ocsp: &[u8], now: UnixTime) -> Result<ServerCertVerified, rustls::Error> {
        let fp = fingerprint(leaf);
        // The very certificate the user trusted: its name and dates were seen when trusting it.
        // The handshake's signatures are still checked against it (below), so only its owner
        // can use it.
        if self.pinned.as_deref() == Some(fp.as_str()) {
            return Ok(ServerCertVerified::assertion());
        }
        self.usual.verify_server_cert(leaf, intermediates, name, ocsp, now).inspect_err(|e| {
            *self.seen.lock().unwrap_or_else(|e| e.into_inner()) = Some(Rejected { fingerprint: fp, why: why(e) });
        })
    }

    fn verify_tls12_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.algorithms)
    }

    fn verify_tls13_signature(&self, message: &[u8], cert: &CertificateDer<'_>, dss: &DigitallySignedStruct) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.algorithms.supported_schemes()
    }
}

/// The TLS setup of a connection that trusts what `trust` says.
pub(crate) fn config(trust: &Trust) -> Arc<ClientConfig> {
    // The authorities and their verifier never change: made once.
    static USUAL: std::sync::OnceLock<(Arc<CryptoProvider>, Arc<WebPkiServerVerifier>)> = std::sync::OnceLock::new();
    let (provider, usual) = USUAL.get_or_init(|| {
        let provider: Arc<CryptoProvider> = Arc::new(rustls::crypto::ring::default_provider());
        let roots = Arc::new(rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() });
        let usual = WebPkiServerVerifier::builder_with_provider(roots, provider.clone()).build().expect("the built-in authorities make a verifier");
        (provider, usual)
    });
    let (provider, usual) = (provider.clone(), usual.clone());
    let verifier = Verifier { usual, algorithms: provider.signature_verification_algorithms, pinned: trust.pinned.clone(), seen: trust.seen.clone() };
    let cfg = ClientConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .expect("ring supports the default TLS versions")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(verifier))
        .with_no_client_auth();
    Arc::new(cfg)
}

/// ureq's TLS step with [`config`]'s verifier: the last link of the WebDAV agent's connector
/// chain, after TCP.
pub(crate) struct Connector {
    pub config: Arc<ClientConfig>,
}

impl fmt::Debug for Connector {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("tls::Connector")
    }
}

impl<In: Transport> ureq::unversioned::transport::Connector<In> for Connector {
    type Out = Either<In, Tls>;

    fn connect(&self, details: &ConnectionDetails, chained: Option<In>) -> Result<Option<Self::Out>, ureq::Error> {
        let Some(transport) = chained else { return Ok(None) };
        if !details.needs_tls() || transport.is_tls() {
            return Ok(Some(Either::A(transport)));
        }
        let host = details.uri.host().unwrap_or_default().trim_start_matches('[').trim_end_matches(']').to_string();
        let name = ServerName::try_from(host).map_err(|_| ureq::Error::Tls("nome de servidor inválido"))?;
        let mut conn = ClientConnection::new(self.config.clone(), name)?;
        let mut sock = TransportAdapter::new(transport.boxed(), details.current_time.clone());
        sock.set_timeout(details.timeout);
        conn.complete_io(&mut sock)?;
        let buffers = LazyBuffers::new(details.config.input_buffer_size(), details.config.output_buffer_size());
        Ok(Some(Either::B(Tls { buffers, stream: StreamOwned { conn, sock } })))
    }
}

pub(crate) struct Tls {
    buffers: LazyBuffers,
    stream: StreamOwned<ClientConnection, TransportAdapter>,
}

impl fmt::Debug for Tls {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("tls::Tls")
    }
}

impl Transport for Tls {
    fn buffers(&mut self) -> &mut dyn Buffers {
        &mut self.buffers
    }

    fn transmit_output(&mut self, amount: usize, timeout: NextTimeout) -> Result<(), ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        let output = &self.buffers.output()[..amount];
        self.stream.write_all(output)?;
        Ok(())
    }

    fn await_input(&mut self, timeout: NextTimeout) -> Result<bool, ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        let input = self.buffers.input_append_buf();
        let amount = self.stream.read(input)?;
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }

    fn is_open(&mut self) -> bool {
        self.stream.get_mut().get_mut().is_open()
    }

    fn is_tls(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_read_as_browsers_show_them() {
        let fp = fingerprint(b"abc");
        assert_eq!(fp.len(), 32 * 3 - 1);
        assert!(fp.starts_with("BA:78:16:BF:"), "{fp}");
    }
}
