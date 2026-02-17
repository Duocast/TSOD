use anyhow::{anyhow, Result};
use quinn::{ClientConfig, Endpoint};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use std::{net::SocketAddr, sync::Arc};

pub fn make_endpoint(listen: &str, server_name: &str, pin_hex: Option<String>, insecure: bool) -> Result<(Endpoint, ServerName<'static>)> {
    let addr: SocketAddr = listen.parse()?;
    let mut ep = Endpoint::client(addr)?;

    let sn = ServerName::try_from(server_name.to_string()).map_err(|_| anyhow!("bad server_name"))?;

    let cfg = if let Some(pin_hex) = pin_hex {
        let pin = hex_to_32(&pin_hex)?;
        pinned_client_config(pin)?
    } else if insecure {
        insecure_client_config()?
    } else {
        return Err(anyhow!("TLS: must provide --pin-sha256-hex (or VP_TLS_PIN_SHA256_HEX) or use --insecure explicitly"));
    };

    ep.set_default_client_config(cfg);
    Ok((ep, sn))
}

fn pinned_client_config(pin_sha256: [u8; 32]) -> Result<ClientConfig> {
    struct Pinner { pin: [u8; 32] }

    impl rustls::client::danger::ServerCertVerifier for Pinner {
        fn verify_server_cert(
            &self,
            end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp: &[u8],
            _now: UnixTime,
        ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            let digest = ring::digest::digest(&ring::digest::SHA256, end_entity.as_ref());
            if digest.as_ref() == self.pin {
                Ok(rustls::client::danger::ServerCertVerified::assertion())
            } else {
                Err(rustls::Error::General("cert pin mismatch".into()))
            }
        }
    }

    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(Pinner { pin: pin_sha256 }))
        .with_no_client_auth();

    Ok(ClientConfig::new(Arc::new(quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?)))
}

fn insecure_client_config() -> Result<ClientConfig> {
    struct AcceptAny;
    impl rustls::client::danger::ServerCertVerifier for AcceptAny {
        fn verify_server_cert(
            &self,
            _end_entity: &CertificateDer<'_>,
            _intermediates: &[CertificateDer<'_>],
            _server_name: &ServerName<'_>,
            _ocsp: &[u8],
            _now: UnixTime,
        ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
            Ok(rustls::client::danger::ServerCertVerified::assertion())
        }
    }

    let crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(AcceptAny))
        .with_no_client_auth();

    Ok(ClientConfig::new(Arc::new(quinn::crypto::rustls::QuicClientConfig::try_from(crypto)?)))
}

fn hex_to_32(s: &str) -> Result<[u8; 32]> {
    let s = s.trim();
    if s.len() != 64 {
        return Err(anyhow!("expected 64 hex chars, got {}", s.len()));
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        out[i] = u8::from_str_radix(&s[i * 2..i * 2 + 2], 16)
            .map_err(|_| anyhow!("invalid hex at byte {}", i))?;
    }
    Ok(out)
}
