use anyhow::{anyhow, Context, Result};
use rcgen::{CertificateParams, DnType, KeyPair, SanType};
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use std::net::IpAddr;

const DEFAULT_SELF_SIGNED_DNS_SAN: &str = "localhost";
const DEFAULT_SELF_SIGNED_IP_SANS: [IpAddr; 2] = [
    IpAddr::V4(std::net::Ipv4Addr::LOCALHOST),
    IpAddr::V6(std::net::Ipv6Addr::LOCALHOST),
];

pub fn load_or_generate_tls(
    cert_pem: Option<&str>,
    key_pem: Option<&str>,
    extra_self_signed_sans: &[String],
) -> Result<(Vec<CertificateDer<'static>>, PrivateKeyDer<'static>)> {
    match (cert_pem, key_pem) {
        (Some(cert_path), Some(key_path)) => {
            let cert_data = std::fs::read(cert_path).context("read cert PEM")?;
            let key_data = std::fs::read(key_path).context("read key PEM")?;

            let certs: Vec<CertificateDer<'static>> = rustls_pemfile::certs(&mut &cert_data[..])
                .collect::<std::result::Result<Vec<_>, _>>()
                .context("parse cert PEM")?;

            if certs.is_empty() {
                return Err(anyhow!("no certificates found in {}", cert_path));
            }

            let key = rustls_pemfile::private_key(&mut &key_data[..])
                .context("parse key PEM")?
                .ok_or_else(|| anyhow!("no private key found in {}", key_path))?;

            Ok((certs, key))
        }
        (None, None) => {
            let cert = generate_self_signed(extra_self_signed_sans)
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

fn generate_self_signed(extra_self_signed_sans: &[String]) -> Result<rcgen::CertifiedKey<KeyPair>> {
    let mut params = CertificateParams::new(vec![DEFAULT_SELF_SIGNED_DNS_SAN.to_string()])?;
    params
        .distinguished_name
        .push(DnType::CommonName, DEFAULT_SELF_SIGNED_DNS_SAN);

    for ip in DEFAULT_SELF_SIGNED_IP_SANS {
        params.subject_alt_names.push(SanType::IpAddress(ip));
    }

    for san in extra_self_signed_sans {
        let san = san.trim();
        if san.is_empty() {
            continue;
        }

        if let Ok(ip) = san.parse::<IpAddr>() {
            params.subject_alt_names.push(SanType::IpAddress(ip));
        } else {
            params.subject_alt_names.push(SanType::DnsName(
                san.try_into()
                    .map_err(|_| anyhow!("invalid DNS SAN value: {}", san))?,
            ));
        }
    }

    let key_pair = KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;
    Ok(rcgen::CertifiedKey { cert, key_pair })
}
