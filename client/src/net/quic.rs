use anyhow::Result;
use quinn::{ClientConfig, Endpoint, TransportConfig};
use std::{net::SocketAddr, sync::Arc};

pub const QUIC_MAX_DATAGRAM_SIZE: usize = vp_voice::QUIC_MAX_DATAGRAM_BYTES;
const QUIC_DATAGRAM_RECV_BUFFER_SIZE: usize = QUIC_MAX_DATAGRAM_SIZE;
const QUIC_DATAGRAM_SEND_BUFFER_SIZE: usize = 1024 * 1024;

pub fn client_config_with_transport(crypto: rustls::ClientConfig) -> Result<ClientConfig> {
    let mut cfg = ClientConfig::new(Arc::new(quinn::crypto::rustls::QuicClientConfig::try_from(
        crypto,
    )?));
    let mut transport = TransportConfig::default();
    // In quinn 0.11, max_datagram_frame_size is advertised from datagram_receive_buffer_size.
    transport.datagram_receive_buffer_size(Some(QUIC_DATAGRAM_RECV_BUFFER_SIZE));
    transport.datagram_send_buffer_size(QUIC_DATAGRAM_SEND_BUFFER_SIZE);
    cfg.transport_config(Arc::new(transport));
    Ok(cfg)
}

pub fn make_ca_endpoint(ca_cert_path: &str, alpn: &str) -> Result<Endpoint> {
    let ca_pem = std::fs::read(ca_cert_path)?;
    let mut root_store = rustls::RootCertStore::empty();
    for cert in rustls_pemfile::certs(&mut &ca_pem[..]) {
        root_store.add(cert?)?;
    }
    let mut crypto = rustls::ClientConfig::builder()
        .with_root_certificates(root_store)
        .with_no_client_auth();
    crypto.alpn_protocols = vec![alpn.as_bytes().to_vec()];

    let mut endpoint = Endpoint::client("[::]:0".parse::<SocketAddr>()?)?;
    endpoint.set_default_client_config(client_config_with_transport(crypto)?);
    Ok(endpoint)
}
