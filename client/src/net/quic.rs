use anyhow::Result;
use quinn::{ClientConfig, Endpoint};
use rustls::pki_types::ServerName;
use std::{net::SocketAddr, sync::Arc};

pub fn make_endpoint() -> Result<Endpoint> {
    let mut endpoint = Endpoint::client("[::]:0".parse::<SocketAddr>()?)?;
    endpoint.set_default_client_config(make_client_config()?);
    Ok(endpoint)
}

pub fn make_ca_endpoint(ca_cert_path: &str) -> Result<Endpoint> {
    let ca_pem = std::fs::read(ca_cert_path)?;
    let mut root_store = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut &ca_pem[..]) {
        root_store.add(cert?)?;
    }
    let crypto = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    let mut endpoint = Endpoint::client("[::]:0".parse::<SocketAddr>()?)?;
    endpoint.set_default_client_config(ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?,
    )));
    Ok(endpoint)
}

fn make_client_config() -> Result<ClientConfig> {
    // Dev-mode: accept any cert (NOT production). Replace with pinning/CA.
    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAnyCert))
        .with_no_client_auth();

    Ok(ClientConfig::new(Arc::new(quinn::crypto::rustls::QuicClientConfig::try_from(
        crypto,
    )?)))
}

pub fn server_name(name: &str) -> ServerName<'static> {
    ServerName::try_from(name.to_string()).expect("valid server name")
}

pub fn make_ca_endpoint(ca_cert_path: &str) -> Result<Endpoint> {
    let ca_pem = std::fs::read(ca_cert_path)?;
    let mut root_store = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut &ca_pem[..]) {
        root_store.add(cert?)?;
    }
    let crypto = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();

    let mut endpoint = Endpoint::client("[::]:0".parse::<SocketAddr>()?)?;
    endpoint.set_default_client_config(ClientConfig::new(Arc::new(
        quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?,
    )));
    Ok(endpoint)
}

struct AcceptAnyCert;

impl rustls::client::danger::ServerCertVerifier for AcceptAnyCert {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }
}
