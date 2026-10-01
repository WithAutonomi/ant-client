//! Portable path rules for manifest entries (ADR-0006).
//!
//! A manifest is created on one platform and extracted on any other, so a
//! path is valid only if it is safe everywhere. The same rules run when a
//! manifest is built, when it is decoded, and again before extraction.

use std::collections::HashSet;

use thiserror::Error;
use unicode_normalization::UnicodeNormalization;

/// The only separator a manifest path may use.
pub const PATH_SEPARATOR: char = '/';
/// Longest single component in bytes.
pub const MAX_PATH_COMPONENT_BYTES: usize = 255;
/// Longest whole path in bytes.
pub const MAX_PATH_BYTES: usize = 1024;
/// Characters no component may contain, over and above control characters.
const FORBIDDEN_CHARS: &[char] = &['<', '>', ':', '"', '|', '?', '*', '\\'];
/// Windows reserved device names, compared without regard to case and
/// with any extension removed.
const RESERVED_DEVICE_NAMES: &[&str] = &[
    "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
    "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
];
/// Lowest non-control character.
const FIRST_PRINTABLE: char = '\u{20}';
/// The DEL control character.
const DELETE: char = '\u{7F}';

/// Which portable rule a path broke.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum PathError {
    /// The path is empty.
    #[error("path is empty")]
    Empty,
    /// The path starts with a separator.
    #[error("path must be relative, not start with '/'")]
    LeadingSeparator,
    /// Two separators in a row, or a trailing separator.
    #[error("path has an empty component")]
    EmptyComponent,
    /// A component is `.` or `..`.
    #[error("path component {0:?} is not allowed")]
    DotComponent(String),
    /// A component contains a forbidden character.
    #[error("component {component:?} contains forbidden character {ch:?}")]
    ForbiddenChar {
        /// The component.
        component: String,
        /// The character.
        ch: char,
    },
    /// A component ends with a space or a dot.
    #[error("component {0:?} may not end with a space or a dot")]
    TrailingSpaceOrDot(String),
    /// A component is a Windows reserved device name.
    #[error("component {0:?} is a reserved device name")]
    ReservedName(String),
    /// A component exceeds [`MAX_PATH_COMPONENT_BYTES`].
    #[error("component {0:?} is longer than {MAX_PATH_COMPONENT_BYTES} bytes")]
    ComponentTooLong(String),
    /// The path exceeds [`MAX_PATH_BYTES`].
    #[error("path is longer than {MAX_PATH_BYTES} bytes")]
    PathTooLong,
    /// Two effective names are equal under the portable comparison.
    #[error("{0:?} collides with another entry")]
    Duplicate(String),
    /// One effective name is a directory prefix of another.
    #[error("{dir:?} is a file but also a directory of {file:?}")]
    PrefixConflict {
        /// The entry that would be a directory.
        dir: String,
        /// The entry beneath it.
        file: String,
    },
}

/// Validate a full relative path.
pub fn validate_path(path: &str) -> Result<(), PathError> {
    if path.is_empty() {
        return Err(PathError::Empty);
    }
    if path.len() > MAX_PATH_BYTES {
        return Err(PathError::PathTooLong);
    }
    if path.starts_with(PATH_SEPARATOR) {
        return Err(PathError::LeadingSeparator);
    }
    for component in path.split(PATH_SEPARATOR) {
        validate_component(component)?;
    }
    Ok(())
}

/// Validate one path component, also used for the manifest `name`.
pub fn validate_component(component: &str) -> Result<(), PathError> {
    if component.is_empty() {
        return Err(PathError::EmptyComponent);
    }
    if component.len() > MAX_PATH_COMPONENT_BYTES {
        return Err(PathError::ComponentTooLong(component.to_string()));
    }
    if component == "." || component == ".." {
        return Err(PathError::DotComponent(component.to_string()));
    }
    if let Some(ch) = component
        .chars()
        .find(|c| *c < FIRST_PRINTABLE || *c == DELETE || FORBIDDEN_CHARS.contains(c))
    {
        return Err(PathError::ForbiddenChar {
            component: component.to_string(),
            ch,
        });
    }
    if component.ends_with(' ') || component.ends_with('.') {
        return Err(PathError::TrailingSpaceOrDot(component.to_string()));
    }
    let stem = component.split('.').next().unwrap_or(component);
    if RESERVED_DEVICE_NAMES
        .iter()
        .any(|reserved| stem.eq_ignore_ascii_case(reserved))
    {
        return Err(PathError::ReservedName(component.to_string()));
    }
    Ok(())
}

/// The key two paths are compared under: NFC normalised and lowercased.
///
/// Lowercasing approximates Unicode case folding closely enough to reject
/// anything a case-insensitive filesystem would merge.
pub fn fold(path: &str) -> String {
    path.nfc().collect::<String>().to_lowercase()
}

/// Reject duplicate effective names and file-versus-directory conflicts
/// under the folded comparison.
pub fn check_collisions<'a>(names: impl IntoIterator<Item = &'a str>) -> Result<(), PathError> {
    let originals: Vec<&str> = names.into_iter().collect();
    let folded: Vec<String> = originals.iter().map(|n| fold(n)).collect();

    let mut seen: HashSet<&str> = HashSet::with_capacity(folded.len());
    for (i, key) in folded.iter().enumerate() {
        if !seen.insert(key.as_str()) {
            return Err(PathError::Duplicate(originals[i].to_string()));
        }
    }
    for (i, key) in folded.iter().enumerate() {
        for (end, _) in key.match_indices(PATH_SEPARATOR) {
            let prefix = &key[..end];
            if seen.contains(prefix) {
                let dir = folded
                    .iter()
                    .position(|k| k == prefix)
                    .map(|j| originals[j])
                    .unwrap_or(prefix);
                return Err(PathError::PrefixConflict {
                    dir: dir.to_string(),
                    file: originals[i].to_string(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn accepts_ordinary_paths() {
        for p in [
            "a",
            "a/b/c.txt",
            ".hidden",
            "dir.with.dots/file",
            "unicode/ñandú.txt",
            "spaces inside/are fine.md",
        ] {
            assert_eq!(validate_path(p), Ok(()), "{p}");
        }
    }

    #[test]
    fn rejects_each_rule() {
        assert_eq!(validate_path(""), Err(PathError::Empty));
        assert_eq!(validate_path("/abs"), Err(PathError::LeadingSeparator));
        assert_eq!(validate_path("a//b"), Err(PathError::EmptyComponent));
        assert_eq!(validate_path("a/"), Err(PathError::EmptyComponent));
        assert_eq!(
            validate_path("a/./b"),
            Err(PathError::DotComponent(".".into()))
        );
        assert_eq!(
            validate_path("../b"),
            Err(PathError::DotComponent("..".into()))
        );
        assert!(matches!(
            validate_path("a\\b"),
            Err(PathError::ForbiddenChar { ch: '\\', .. })
        ));
        assert!(matches!(
            validate_path("C:/x"),
            Err(PathError::ForbiddenChar { ch: ':', .. })
        ));
        assert!(matches!(
            validate_path("nul\0byte"),
            Err(PathError::ForbiddenChar { ch: '\0', .. })
        ));
        assert!(matches!(
            validate_path("del\u{7f}"),
            Err(PathError::ForbiddenChar { ch: '\u{7f}', .. })
        ));
        assert_eq!(
            validate_path("trailing. "),
            Err(PathError::TrailingSpaceOrDot("trailing. ".into()))
        );
        assert_eq!(
            validate_path("trailing."),
            Err(PathError::TrailingSpaceOrDot("trailing.".into()))
        );
        assert_eq!(
            validate_path("dir/CON.txt"),
            Err(PathError::ReservedName("CON.txt".into()))
        );
        assert_eq!(
            validate_path("lpt9"),
            Err(PathError::ReservedName("lpt9".into()))
        );
        assert_eq!(validate_path("COM10"), Ok(()));
        let long = "x".repeat(MAX_PATH_COMPONENT_BYTES + 1);
        assert!(matches!(
            validate_path(&long),
            Err(PathError::ComponentTooLong(_))
        ));
        let deep = std::iter::repeat_n("a", MAX_PATH_BYTES)
            .collect::<Vec<_>>()
            .join("/");
        assert_eq!(validate_path(&deep), Err(PathError::PathTooLong));
    }

    #[test]
    fn collisions_fold_case_and_normalisation() {
        assert_eq!(check_collisions(["a", "b", "c/d"]), Ok(()));
        assert_eq!(
            check_collisions(["Readme", "readme"]),
            Err(PathError::Duplicate("readme".into()))
        );
        // "é" precomposed versus "e" + combining acute.
        assert_eq!(
            check_collisions(["caf\u{e9}", "cafe\u{301}"]),
            Err(PathError::Duplicate("cafe\u{301}".into()))
        );
    }

    #[test]
    fn prefix_conflicts_are_detected_across_case() {
        assert!(matches!(
            check_collisions(["a", "a/b"]),
            Err(PathError::PrefixConflict { .. })
        ));
        assert!(matches!(
            check_collisions(["a/b", "A"]),
            Err(PathError::PrefixConflict { .. })
        ));
        assert!(matches!(
            check_collisions(["x/y/z", "X/Y"]),
            Err(PathError::PrefixConflict { .. })
        ));
        assert_eq!(check_collisions(["a", "a-b", "ab/c"]), Ok(()));
    }
}
