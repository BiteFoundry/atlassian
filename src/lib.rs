//! `atlassian_cli` — Unified Atlassian Jira & Confluence native terminal client (`atlassian` / `confluence` / `conf`).

pub mod adf;
pub mod atlassian;
pub mod auth;
pub mod cli;
pub mod config;
pub mod daemon;
pub mod img_preview;
pub mod mcp;
pub mod models;
pub mod resolver;
pub mod serve;
pub mod shell_alias;
pub mod storage;
pub mod sync;
pub mod tui;
pub mod ui;
pub mod updater;

/// Encodes a byte slice as lowercase hexadecimal.
pub fn encode_hex(bytes: impl AsRef<[u8]>) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = bytes.as_ref();
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// Generates an RFC 4122 version 4 (random) UUID string in lowercase hyphenated form.
pub fn new_uuid_v4() -> String {
    use rand::RngCore;

    let mut bytes = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;

    let hex = encode_hex(bytes);
    format!(
        "{}-{}-{}-{}-{}",
        &hex[0..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..32]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_uuid_v4_matches_rfc4122_format() {
        let a = new_uuid_v4();
        let b = new_uuid_v4();
        assert_ne!(a, b);
        assert_eq!(a.len(), 36);
        let bytes = a.as_bytes();
        assert_eq!(bytes[8], b'-');
        assert_eq!(bytes[13], b'-');
        assert_eq!(bytes[18], b'-');
        assert_eq!(bytes[23], b'-');
        assert_eq!(bytes[14], b'4');
        assert!(matches!(bytes[19], b'8' | b'9' | b'a' | b'b'));
        assert!(a.chars().all(|c| c == '-' || matches!(c, '0'..='9' | 'a'..='f')));
    }
}
