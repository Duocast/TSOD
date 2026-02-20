use anyhow::{anyhow, Context, Result};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

pub fn load_or_generate_tls(
    cert_pem: Option<&str>,
    key_pem: Option<&str>,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    match (cert_pem, key_pem) {
        (Some(_), Some(_)) => Err(anyhow!(
            "PEM loading is not wired in this build; run without --tls-cert-pem/--tls-key-pem for dev self-signed certs"
        )),
        (None, None) => {
            let cert = generate_simple_self_signed(vec!["localhost".into()])
                .context("failed generating self-signed cert")?;
            let cert_der: CertificateDer<'static> = cert.cert.der().clone();
            let key_der = PrivateKeyDer::Pkcs8(cert.key_pair.serialize_der().into());
            Ok((vec![cert_der], key_der))
        }
        _ => Err(anyhow!(
            "must set both --tls-cert-pem and --tls-key-pem, or neither"
        )),
    }
}
