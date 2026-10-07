//! `ant://` links (ADR-0006).
//!
//! Two forms, told apart by the authority:
//!
//! ```text
//! ant://<64 hex characters>                       a public file's DataMap address
//! ant://manifest/<unpadded base64url .ant bytes>  a manifest, bytes included
//! ```
//!
//! A bare 64-character hex address is accepted wherever a link is. A file
//! link carries nothing but the address: any query string or path on it is
//! rejected so that grammar stays reserved.

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine;

use super::{Manifest, ManifestError, ADDRESS_LEN};

#[cfg(test)]
use super::{ContentRef, ManifestEntry};

/// Scheme prefix of every link.
pub const LINK_SCHEME: &str = "ant://";
/// Authority that marks a manifest link.
pub const MANIFEST_AUTHORITY: &str = "manifest";
/// Prefix of a manifest link.
pub const MANIFEST_LINK_PREFIX: &str = "ant://manifest/";
/// Characters that would start a path, query or fragment on a file link.
const FILE_LINK_TERMINATORS: &[char] = &['/', '?', '#'];
/// Hex characters in an address.
const ADDRESS_HEX_LEN: usize = ADDRESS_LEN * 2;

/// What a link points at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Link {
    /// A public file, by its DataMap address.
    File([u8; ADDRESS_LEN]),
    /// A manifest carried in the link itself.
    Manifest(Manifest),
}

/// Whether `input` starts with the `ant://` scheme, ignoring case.
pub fn is_link(input: &str) -> bool {
    input
        .get(..LINK_SCHEME.len())
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case(LINK_SCHEME))
}

/// Parse a link or a bare hex address.
pub fn parse_link(input: &str) -> Result<Link, ManifestError> {
    let input = input.trim();
    if !is_link(input) {
        return parse_address(input).map(Link::File);
    }
    let rest = &input[LINK_SCHEME.len()..];

    let authority_end = rest.find(FILE_LINK_TERMINATORS).unwrap_or(rest.len());
    let authority = &rest[..authority_end];
    let remainder = &rest[authority_end..];

    if authority.eq_ignore_ascii_case(MANIFEST_AUTHORITY) {
        let payload = remainder.strip_prefix('/').ok_or_else(|| {
            ManifestError::Link("manifest link is missing its payload".to_string())
        })?;
        let bytes = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|e| ManifestError::Link(format!("manifest payload is not base64url: {e}")))?;
        return Manifest::decode(&bytes).map(Link::Manifest);
    }

    if !remainder.is_empty() {
        return Err(ManifestError::Link(
            "a file link carries only an address; paths and query strings are not allowed"
                .to_string(),
        ));
    }
    parse_address(authority).map(Link::File)
}

/// Format a file link.
pub fn file_link(address: &[u8; ADDRESS_LEN]) -> String {
    format!("{LINK_SCHEME}{}", hex::encode(address))
}

/// Format a manifest link, encoding the manifest.
pub fn manifest_link(manifest: &Manifest) -> Result<String, ManifestError> {
    Ok(manifest_link_from_bytes(&manifest.encode()?))
}

/// Format a manifest link from already-encoded `.ant` bytes.
pub fn manifest_link_from_bytes(bytes: &[u8]) -> String {
    format!("{MANIFEST_LINK_PREFIX}{}", URL_SAFE_NO_PAD.encode(bytes))
}

fn parse_address(hex_str: &str) -> Result<[u8; ADDRESS_LEN], ManifestError> {
    if hex_str.len() != ADDRESS_HEX_LEN {
        return Err(ManifestError::Link(format!(
            "address must be {ADDRESS_HEX_LEN} hex characters, got {}",
            hex_str.len()
        )));
    }
    let bytes = hex::decode(hex_str)
        .map_err(|e| ManifestError::Link(format!("address is not hex: {e}")))?;
    let mut out = [0u8; ADDRESS_LEN];
    out.copy_from_slice(&bytes);
    Ok(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn sample() -> Manifest {
        Manifest {
            name: Some("pack".into()),
            torrent: None,
            entries: vec![ManifestEntry {
                path: Some("a.bin".into()),
                size: Some(1),
                source: ContentRef::Public { address: [5; 32] },
            }],
        }
    }

    #[test]
    fn file_link_round_trips_and_accepts_bare_hex() {
        let addr = [0xabu8; 32];
        let link = file_link(&addr);
        assert_eq!(parse_link(&link).unwrap(), Link::File(addr));
        assert_eq!(parse_link(&hex::encode(addr)).unwrap(), Link::File(addr));
        assert_eq!(
            parse_link(&link.to_uppercase()).unwrap(),
            Link::File(addr),
            "scheme and hex are case-insensitive"
        );
    }

    #[test]
    fn file_link_rejects_paths_queries_and_bad_addresses() {
        let hex64 = "ab".repeat(32);
        for bad in [
            format!("ant://{hex64}/x"),
            format!("ant://{hex64}?dn=x"),
            format!("ant://{hex64}#frag"),
            "ant://abc".to_string(),
            "ant://".to_string(),
            "zz".repeat(32),
            format!("ant://{}", "zz".repeat(32)),
        ] {
            assert!(
                matches!(parse_link(&bad), Err(ManifestError::Link(_))),
                "{bad}"
            );
        }
    }

    #[test]
    fn manifest_link_round_trips() {
        let link = manifest_link(&sample()).unwrap();
        assert!(link.starts_with(MANIFEST_LINK_PREFIX));
        match parse_link(&link).unwrap() {
            Link::Manifest(m) => assert_eq!(m, sample()),
            Link::File(_) => panic!("expected manifest"),
        }
    }

    #[test]
    fn manifest_link_rejects_non_manifest_payload() {
        let payload = URL_SAFE_NO_PAD.encode(b"not a manifest at all");
        assert!(matches!(
            parse_link(&format!("{MANIFEST_LINK_PREFIX}{payload}")),
            Err(ManifestError::BadHeader)
        ));
        assert!(matches!(
            parse_link("ant://manifest/!!!"),
            Err(ManifestError::Link(_))
        ));
        assert!(matches!(
            parse_link("ant://manifest"),
            Err(ManifestError::Link(_))
        ));
    }
}
