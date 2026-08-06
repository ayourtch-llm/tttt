//! Self-signed certificate generation for the web UI (`--secure`).
//!
//! Generates an in-memory self-signed X.509 certificate + private key when the
//! user requests TLS without supplying their own cert/key. The generated cert
//! is not persisted — a fresh one is minted on each start.

/// A generated certificate + private key (DER encoded for rustls).
pub struct GeneratedCert {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

/// Generate a self-signed certificate valid for the given hostnames.
pub fn generate_self_signed(hosts: &[String]) -> Result<GeneratedCert, Box<dyn std::error::Error>> {
    let mut params = rcgen::CertificateParams::new(hosts.to_vec())?;
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "tttt self-signed");
    params
        .distinguished_name
        .push(rcgen::DnType::OrganizationName, "tttt");
    params.not_before = rcgen::date_time_ymd(2020, 1, 1);
    params.not_after = rcgen::date_time_ymd(2030, 1, 1);
    params.is_ca = rcgen::IsCa::NoCa;
    params.use_authority_key_identifier_extension = false;

    let key_pair = rcgen::KeyPair::generate()?;
    let cert = params.self_signed(&key_pair)?;

    Ok(GeneratedCert {
        cert_der: cert.der().to_vec(),
        key_der: key_pair.serialize_der(),
    })
}

/// Build a rustls ServerConfig from a generated cert, or from user-supplied
/// PEM cert/key files when provided.
pub fn build_server_config(
    generated: Option<&GeneratedCert>,
    cert_pem: Option<&std::path::Path>,
    key_pem: Option<&std::path::Path>,
) -> Result<rustls::ServerConfig, Box<dyn std::error::Error>> {
    let cert_chain: Vec<rustls::pki_types::CertificateDer<'static>>;
    let key: rustls::pki_types::PrivateKeyDer<'static>;

    if let (Some(cp), Some(kp)) = (cert_pem, key_pem) {
        // User-supplied PEM files
        let certs_pem = std::fs::read(cp)?;
        let key_pem_data = std::fs::read(kp)?;
        cert_chain = rustls_pemfile::certs(&mut &*certs_pem)
            .collect::<Result<Vec<_>, _>>()?;
        key = rustls_pemfile::private_key(&mut &*key_pem_data)?
            .ok_or("no private key found in key file")?;
    } else if let Some(g) = generated {
        // rcgen's serialize_der() emits a PKCS#8 key.
        cert_chain = vec![rustls::pki_types::CertificateDer::from(g.cert_der.clone())];
        key = rustls::pki_types::PrivateKeyDer::Pkcs8(
            rustls::pki_types::PrivatePkcs8KeyDer::from(g.key_der.clone()),
        );
    } else {
        return Err("TLS requested but no certificate available".into());
    }

    let config = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(cert_chain, key)?;
    Ok(config)
}
