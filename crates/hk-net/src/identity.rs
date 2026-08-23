use std::fs;
use std::path::Path;

use hk_core::device::Fingerprint;
use rustls::pki_types::{CertificateDer, PrivatePkcs8KeyDer};

use crate::Error;

/// This device's long-lived certificate and private key.
pub struct Identity {
    pub(crate) certificate: CertificateDer<'static>,
    pub(crate) private_key: PrivatePkcs8KeyDer<'static>,
    fingerprint: Fingerprint,
}

impl Identity {
    /// Creates a new self-signed identity.
    pub fn generate() -> Result<Self, Error> {
        let generated = rcgen::generate_simple_self_signed(vec!["hackerman".to_string()])?;
        Ok(Self::from_der(
            generated.cert.der().to_vec(),
            generated.signing_key.serialize_der(),
        ))
    }

    /// Loads the identity saved in `dir`, or creates and saves one on first run.
    pub fn load_or_create(dir: &Path) -> Result<Self, Error> {
        let certificate_path = dir.join("certificate.der");
        let key_path = dir.join("private-key.der");
        if certificate_path.exists() && key_path.exists() {
            return Ok(Self::from_der(
                fs::read(certificate_path)?,
                fs::read(key_path)?,
            ));
        }
        let identity = Self::generate()?;
        fs::create_dir_all(dir)?;
        fs::write(&key_path, identity.private_key.secret_pkcs8_der())?;
        fs::write(&certificate_path, identity.certificate.as_ref())?;
        Ok(identity)
    }

    fn from_der(certificate: Vec<u8>, private_key: Vec<u8>) -> Self {
        Self {
            fingerprint: fingerprint_of(&certificate),
            certificate: certificate.into(),
            private_key: private_key.into(),
        }
    }

    pub fn fingerprint(&self) -> Fingerprint {
        self.fingerprint
    }
}

/// The fingerprint of a certificate in DER form.
pub fn fingerprint_of(certificate: &[u8]) -> Fingerprint {
    let digest = ring::digest::digest(&ring::digest::SHA256, certificate);
    let mut bytes = [0; 32];
    bytes.copy_from_slice(digest.as_ref());
    Fingerprint::from_bytes(bytes)
}
