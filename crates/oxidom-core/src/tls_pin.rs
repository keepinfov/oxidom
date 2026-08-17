//! Reading a server's certificate, so a person can decide whether to trust it.
//!
//! Xray 26 removed `allowInsecure`: a server with a self-signed certificate is
//! now unreachable unless its certificate is *pinned*, and a pin is a hash
//! nobody can type from memory. This module fetches the hash so the app can
//! show it and ask — the same bargain ssh makes on a first connection, and a
//! strictly better one than `allowInsecure` ever offered, which accepted any
//! certificate every time rather than one certificate once.
//!
//! Nothing here decides anything. It reads what the server presents and hands
//! back a fingerprint; trusting it is a separate, explicit act.

use std::net::{TcpStream, ToSocketAddrs};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, IpAddr, ServerName, UnixTime};
use rustls::{ClientConfig, ClientConnection, DigitallySignedStruct, SignatureScheme};

/// The handshake exists only to see the certificate, so it gets the same
/// budget as a probe rather than a browser's patience.
const TIMEOUT: Duration = Duration::from_secs(5);

/// A certificate as the user will be asked about it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Presented {
    /// SHA-256 of the leaf certificate's DER, lowercase hex — the exact string
    /// Xray takes as `pinnedPeerCertSha256`.
    pub sha256: String,
    /// Where it was read from, so a dialog can say what it is talking about.
    pub host: String,
    pub port: u16,
}

impl Presented {
    /// Grouped in pairs, the way every tool that shows a fingerprint does,
    /// because sixty-four unbroken characters cannot be compared by eye.
    pub fn readable(&self) -> String {
        self.sha256
            .as_bytes()
            .chunks(2)
            .map(|pair| String::from_utf8_lossy(pair).to_string())
            .collect::<Vec<_>>()
            .join(":")
    }
}

/// Complete a TLS handshake with `host:port` without judging the certificate,
/// and report what it presented.
///
/// `sni` is what the server is asked to identify itself as, which for a
/// fronted server is not its address.
pub fn present(host: &str, port: u16, sni: Option<&str>) -> Result<Presented> {
    let name = server_name(sni.unwrap_or(host))?;
    let address = (host, port)
        .to_socket_addrs()
        .with_context(|| format!("resolving {host}"))?
        .next()
        .ok_or_else(|| anyhow!("{host} resolved to no address"))?;

    let mut socket = TcpStream::connect_timeout(&address, TIMEOUT)
        .with_context(|| format!("reaching {host}"))?;
    socket.set_read_timeout(Some(TIMEOUT))?;
    socket.set_write_timeout(Some(TIMEOUT))?;

    // The provider is named rather than taken from the process default: this
    // library is linked into a daemon that never installs one, and
    // `ClientConfig::builder()` panics when none is set.
    let config =
        ClientConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .context("building a TLS client")?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAnything))
            .with_no_client_auth();

    let mut connection =
        ClientConnection::new(Arc::new(config), name).context("starting a TLS handshake")?;
    // Drives the handshake and nothing else: no request is sent, and the
    // connection is dropped as soon as the certificate has been read.
    connection
        .complete_io(&mut socket)
        .with_context(|| format!("handshaking with {host}"))?;

    let Some(certificate) = connection.peer_certificates().and_then(<[_]>::first) else {
        bail!("{host} completed a handshake without presenting a certificate");
    };
    Ok(Presented {
        sha256: sha256_hex(certificate),
        host: host.to_string(),
        port,
    })
}

fn sha256_hex(certificate: &CertificateDer<'_>) -> String {
    let digest = ring::digest::digest(&ring::digest::SHA256, certificate.as_ref());
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn server_name(name: &str) -> Result<ServerName<'static>> {
    if let Ok(address) = name.parse::<std::net::IpAddr>() {
        return Ok(ServerName::IpAddress(IpAddr::from(address)));
    }
    ServerName::try_from(name.to_string()).with_context(|| format!("{name} is not a server name"))
}

/// Accepts every certificate, because judging it is the caller's job — and in
/// this module the caller is a person being shown a fingerprint.
///
/// This verifier is never used to *carry* traffic: the connection it belongs
/// to is closed the moment the certificate has been read.
#[derive(Debug)]
struct AcceptAnything;

impl ServerCertVerifier for AcceptAnything {
    fn verify_server_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &CertificateDer<'_>,
        _dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        Ok(HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        rustls::crypto::ring::default_provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_fingerprint_is_shown_in_pairs() {
        let presented = Presented {
            sha256: "2bda9443d0df5995".to_string(),
            host: "example.net".to_string(),
            port: 443,
        };
        assert_eq!(presented.readable(), "2b:da:94:43:d0:df:59:95");
    }

    /// The pin Xray takes is lowercase hex of the DER, and the digest of the
    /// empty input is the one value every SHA-256 implementation agrees on.
    #[test]
    fn the_hash_is_lowercase_hex_of_the_certificate_bytes() {
        let empty = CertificateDer::from(Vec::new());
        assert_eq!(
            sha256_hex(&empty),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    /// The only check that proves this against a real handshake. Opt-in
    /// because it needs a server:
    ///
    /// ```sh
    /// OXIDOM_TEST_TLS_ENDPOINT=127.0.0.1:19001 \
    ///   cargo test -p oxidom-core -- --ignored reads_a_certificate
    /// ```
    ///
    /// Compare what it prints with
    /// `openssl x509 -in cert.pem -outform der | openssl dgst -sha256`.
    #[test]
    #[ignore = "requires a TLS server in OXIDOM_TEST_TLS_ENDPOINT"]
    fn reads_a_certificate_from_a_live_server() {
        let endpoint = std::env::var("OXIDOM_TEST_TLS_ENDPOINT").expect("an endpoint");
        let (host, port) = endpoint.rsplit_once(':').expect("host:port");
        let presented = present(host, port.parse().expect("a port"), None).expect("a certificate");
        println!("{} presented {}", endpoint, presented.readable());
        assert_eq!(
            presented.sha256.len(),
            64,
            "sha-256 as hex is 64 characters"
        );
        assert!(presented.sha256.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_name_may_be_a_host_or_a_literal_address() {
        assert!(server_name("example.net").is_ok());
        assert!(server_name("198.51.100.7").is_ok());
        assert!(server_name("").is_err());
    }
}
