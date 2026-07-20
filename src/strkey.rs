//! Minimal Stellar strkey validation.
//!
//! A Stellar account address ("strkey") such as `GBBD47IF...` base32-encodes a
//! one-byte version, a 32-byte ed25519 public key, and a two-byte CRC16-XModem
//! checksum. Checking the prefix, length, base32 alphabet and checksum catches
//! the failure mode this guards against: a mistyped `STELLAR_GATEWAY_PUBLIC`
//! (or asset issuer) that would otherwise silently mint unpayable intents.
//!
//! This is a small, dependency-free validator. It verifies structure and the
//! checksum — enough to reject typos and corruption — but does not check that
//! the 32-byte payload is a point on the ed25519 curve.

use std::fmt;

/// Version byte for an ed25519 public-key (account) strkey, which renders as a
/// leading `G`. Defined as `6 << 3` by the SEP-23 strkey encoding.
const ED25519_PUBLIC_KEY_VERSION: u8 = 6 << 3;

/// An ed25519 public-key strkey (`G...`) is always 56 characters: base32 of
/// 1 version + 32 key + 2 checksum = 35 bytes.
const ACCOUNT_STRKEY_LEN: usize = 56;
const ACCOUNT_DECODED_LEN: usize = 35;

/// Why a string is not a valid Stellar account address.
#[derive(Debug, PartialEq, Eq)]
pub enum StrkeyError {
    /// Not the expected 56-character length.
    Length,
    /// Contains a character outside the base32 alphabet `A-Z2-7`.
    Alphabet,
    /// Decoded, but the version byte is not the `G` account-address marker.
    Version,
    /// The trailing CRC16 checksum does not match the payload.
    Checksum,
}

impl fmt::Display for StrkeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let msg = match self {
            StrkeyError::Length => "must be 56 characters",
            StrkeyError::Alphabet => "contains non-base32 characters",
            StrkeyError::Version => "wrong version byte (expected a 'G' account address)",
            StrkeyError::Checksum => "checksum mismatch (corrupted or mistyped)",
        };
        f.write_str(msg)
    }
}

impl std::error::Error for StrkeyError {}

/// Returns `true` if `s` is a structurally valid Stellar account address
/// (`G...`): correct prefix, length, base32 alphabet and CRC16 checksum.
pub fn is_valid_account_id(s: &str) -> bool {
    validate_account_id(s).is_ok()
}

/// Validate a Stellar account address (`G...`), returning the first check that
/// failed so callers can produce a precise error message.
pub fn validate_account_id(s: &str) -> Result<(), StrkeyError> {
    if s.len() != ACCOUNT_STRKEY_LEN {
        return Err(StrkeyError::Length);
    }
    let decoded = base32_decode(s).ok_or(StrkeyError::Alphabet)?;
    if decoded.len() != ACCOUNT_DECODED_LEN {
        return Err(StrkeyError::Length);
    }
    if decoded[0] != ED25519_PUBLIC_KEY_VERSION {
        return Err(StrkeyError::Version);
    }
    let (payload, checksum) = decoded.split_at(decoded.len() - 2);
    let expected = u16::from_le_bytes([checksum[0], checksum[1]]);
    if crc16_xmodem(payload) != expected {
        return Err(StrkeyError::Checksum);
    }
    Ok(())
}

/// Decode an unpadded RFC 4648 base32 string (`A-Z2-7`). Returns `None` if any
/// character is outside the alphabet.
fn base32_decode(s: &str) -> Option<Vec<u8>> {
    const ALPHABET: &[u8; 32] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZ234567";
    let mut out = Vec::with_capacity(s.len() * 5 / 8);
    let mut buffer: u32 = 0;
    let mut bits: u32 = 0;
    for c in s.bytes() {
        let value = ALPHABET.iter().position(|&a| a == c)? as u32;
        buffer = (buffer << 5) | value;
        bits += 5;
        if bits >= 8 {
            bits -= 8;
            out.push((buffer >> bits) as u8);
            buffer &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// CRC16-XModem (polynomial 0x1021, initial value 0x0000) — the checksum
/// Stellar appends, little-endian, to every strkey.
fn crc16_xmodem(data: &[u8]) -> u16 {
    let mut crc: u16 = 0;
    for &byte in data {
        crc ^= (byte as u16) << 8;
        for _ in 0..8 {
            crc = if crc & 0x8000 != 0 {
                (crc << 1) ^ 0x1021
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real, valid Stellar account address (the default USDC issuer).
    const VALID: &str = "GBBD47IF6LWK7P7MDEVSCWR7DPUWV3NY3DTQEVFL4NAT4AQH3ZLLFLA5";

    #[test]
    fn accepts_a_valid_account_id() {
        // Also proves the base32 + CRC16 implementation against a known address.
        assert_eq!(validate_account_id(VALID), Ok(()));
        assert!(is_valid_account_id(VALID));
    }

    #[test]
    fn rejects_corrupted_checksum() {
        /* Flip a character deep in the key body: length and alphabet stay valid,
        but the trailing checksum no longer matches. */
        let mut chars: Vec<char> = VALID.chars().collect();
        chars[20] = if chars[20] == 'A' { 'B' } else { 'A' };
        let corrupted: String = chars.into_iter().collect();
        assert_eq!(corrupted.len(), 56);
        assert_eq!(validate_account_id(&corrupted), Err(StrkeyError::Checksum));
    }

    #[test]
    fn rejects_wrong_version_byte() {
        // Replace the leading 'G' so the version byte no longer marks an account.
        let wrong = format!("A{}", &VALID[1..]);
        assert_eq!(validate_account_id(&wrong), Err(StrkeyError::Version));
    }

    #[test]
    fn rejects_bad_length() {
        assert_eq!(validate_account_id(""), Err(StrkeyError::Length));
        assert_eq!(validate_account_id(&VALID[..55]), Err(StrkeyError::Length));
        assert_eq!(
            validate_account_id(&format!("{VALID}A")),
            Err(StrkeyError::Length)
        );
    }

    #[test]
    fn rejects_non_base32_characters() {
        // '0' is not in the strkey alphabet; keep the length at 56.
        let bad = format!("{}0", &VALID[..55]);
        assert_eq!(bad.len(), 56);
        assert_eq!(validate_account_id(&bad), Err(StrkeyError::Alphabet));
    }

    #[test]
    fn rejects_secret_seed_prefix() {
        // A valid-looking secret seed (S...) is not an account address.
        let seed = "SDJHRQF4GCMIIKAAAQ6IHY42X73FQFLHUULAPSKKD4DFDM7UXWWCRHBE";
        assert_eq!(seed.len(), 56);
        assert!(matches!(
            validate_account_id(seed),
            Err(StrkeyError::Version) | Err(StrkeyError::Checksum)
        ));
    }
}
