//! Device identity.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Deserializer, Serialize, Serializer, de::Error as _};

/// Identifies a device by the SHA-256 hash of its certificate.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub const fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }
}

impl fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(f, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Fingerprint({self})")
    }
}

#[derive(Debug, thiserror::Error)]
#[error("a fingerprint is 64 hexadecimal digits")]
pub struct ParseFingerprintError;

impl FromStr for Fingerprint {
    type Err = ParseFingerprintError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        // Tolerate the separators people add when copying, as in "ab:cd" or "ab cd".
        let digits: Vec<u8> = s
            .bytes()
            .filter(|b| !matches!(b, b':' | b'-' | b' '))
            .collect();
        if digits.len() != 64 {
            return Err(ParseFingerprintError);
        }
        let mut bytes = [0; 32];
        let (pairs, _) = digits.as_chunks::<2>();
        for (byte, &[high, low]) in bytes.iter_mut().zip(pairs) {
            *byte = (hex_value(high)? << 4) | hex_value(low)?;
        }
        Ok(Self(bytes))
    }
}

fn hex_value(digit: u8) -> Result<u8, ParseFingerprintError> {
    match digit {
        b'0'..=b'9' => Ok(digit - b'0'),
        b'a'..=b'f' => Ok(digit - b'a' + 10),
        b'A'..=b'F' => Ok(digit - b'A' + 10),
        _ => Err(ParseFingerprintError),
    }
}

// Saved files and the UI read fingerprints as hex text.
impl Serialize for Fingerprint {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Fingerprint {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        String::deserialize(deserializer)?
            .parse()
            .map_err(D::Error::custom)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: Fingerprint = Fingerprint::from_bytes([0xab; 32]);

    #[test]
    fn displays_and_parses_as_hex() {
        let text = SAMPLE.to_string();
        assert_eq!(text.len(), 64);
        assert_eq!(text.parse::<Fingerprint>().unwrap(), SAMPLE);
    }

    #[test]
    fn parsing_ignores_case_and_separators() {
        let shouty = SAMPLE
            .to_string()
            .to_uppercase()
            .as_bytes()
            .chunks(2)
            .map(|pair| std::str::from_utf8(pair).unwrap())
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(shouty.parse::<Fingerprint>().unwrap(), SAMPLE);
    }

    #[test]
    fn rejects_wrong_length_and_non_hex() {
        assert!("abcd".parse::<Fingerprint>().is_err());
        assert!("g".repeat(64).parse::<Fingerprint>().is_err());
    }

    #[test]
    fn serializes_as_text() {
        let bytes = postcard::to_allocvec(&SAMPLE).unwrap();
        assert_eq!(postcard::from_bytes::<Fingerprint>(&bytes).unwrap(), SAMPLE);
    }
}
