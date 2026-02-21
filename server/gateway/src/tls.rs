use anyhow::{anyhow, Context, Result};
use rcgen::generate_simple_self_signed;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};

pub fn load_or_generate_tls(
    cert_pem: Option<&str>,
    key_pem: Option<&str>,
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
        (Some(cert_path), Some(key_path)) => {
            let cert_pem = std::fs::read(cert_path).context("read cert PEM")?;
            let key_pem = std::fs::read(key_path).context("read key PEM")?;
        
            let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut &cert_pem[..])
                .collect::<Result<Vec<_>, _>>()
                .context("parse cert PEM")?;
        
            let key = rustls_pemfile::private_key(&mut &key_pem[..])
                .context("parse key PEM")?
                .ok_or_else(|| anyhow!("no private key found in PEM"))?;
        
            Ok((certs, key))
        }
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
