use anyhow::{anyhow, Context, Result};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::{fs, path::Path};

pub fn load_or_generate_tls(
    cert_pem: Option<&str>,
    key_pem: Option<&str>,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    match (cert_pem, key_pem) {
        (Some(cert_path), Some(key_path)) => {
            let cert_bytes = fs::read(Path::new(cert_path))
                .with_context(|| format!("failed reading cert PEM: {cert_path}"))?;
            let key_bytes = fs::read(Path::new(key_path))
                .with_context(|| format!("failed reading key PEM: {key_path}"))?;

            let certs = rustls_pemfile::certs(&mut &cert_bytes[..])
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("failed parsing certs PEM")?
                .into_iter()
                .map(|c| CertificateDer::from(c))
                .collect::<Vec<_>>();

            let mut keys = rustls_pemfile::pkcs8_private_keys(&mut &key_bytes[..])
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("failed parsing pkcs8 private keys")?;

            if keys.is_empty() {
                return Err(anyhow("no PKCS8 private key found in tls_key_pem"));
            }

            let key = PrivateKeyDer::from(keys.remove(0));
            Ok((certs, key))
        }
        (None, None) => {
            // Ephemeral self-signed for dev
            let cert = generate_simple_self_signed(vec!["localhost".into()])
                .context("failed generating self-signed cert")?;
            let cert_der = CertificateDer::from(cert.serialize_der().context("cert der")?);
            let key_der = PrivateKeyDer::try_from(cert.serialize_private_key_der())
                .map_err(|_| anyhow!("failed converting private key"))?;
            Ok((vec![cert_der], key_der))
        }
        _ => Err(anyhow!(
            "must set both --tls-cert-pem and --tls-key-pem, or neither"
        )),
    }
}
