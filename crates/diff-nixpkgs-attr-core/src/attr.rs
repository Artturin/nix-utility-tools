//! Attribute paths as given by the user, and names derived from them.

use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum AttrError {
    #[error("at least one attribute is required")]
    None,
    #[error("invalid attribute path: {0}")]
    Invalid(String),
    #[error("invalid system: {0}")]
    InvalidSystem(String),
}

/// Accept flake-style paths: drop a leading `legacyPackages.SYSTEM.`, then a
/// leading `pkgs.`.
pub fn normalize(raw: &str) -> &str {
    let attr = raw
        .strip_prefix("legacyPackages.")
        .and_then(|rest| rest.split_once('.'))
        .filter(|(system, attr)| is_system_triple(system) && !attr.is_empty())
        .map_or(raw, |(_, attr)| attr);
    attr.strip_prefix("pkgs.").unwrap_or(attr)
}

/// `x86_64-linux` and friends: `[A-Za-z0-9_]+-[a-z]+`.
fn is_system_triple(s: &str) -> bool {
    s.split_once('-').is_some_and(|(arch, os)| {
        !arch.is_empty()
            && arch.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
            && !os.is_empty()
            && os.chars().all(|c| c.is_ascii_lowercase())
    })
}

/// `[A-Za-z_][A-Za-z0-9_.-]*`: safe to splice into a Nix expression.
fn is_attr_path(s: &str) -> bool {
    let mut chars = s.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || "_.-".contains(c))
}

/// Normalise, validate and de-duplicate (keeping the first of each) the
/// attributes, before any work is done.
pub fn prepare(raw: &[String]) -> Result<Vec<String>, AttrError> {
    if raw.is_empty() {
        return Err(AttrError::None);
    }
    let mut attrs: Vec<String> = Vec::new();
    for raw in raw {
        let attr = normalize(raw);
        if !is_attr_path(attr) {
            return Err(AttrError::Invalid(raw.clone()));
        }
        if !attrs.iter().any(|seen| seen == attr) {
            attrs.push(attr.to_owned());
        }
    }
    Ok(attrs)
}

/// Systems are spliced into a Nix string: allow only what real ones use.
pub fn validate_system(system: &str) -> Result<(), AttrError> {
    if !system.is_empty()
        && system
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "_-".contains(c))
    {
        Ok(())
    } else {
        Err(AttrError::InvalidSystem(system.to_owned()))
    }
}

/// Replace anything that is awkward in a file name; at most 100 characters.
pub fn sanitize(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || "._+-".contains(c) {
                c
            } else {
                '-'
            }
        })
        .take(100)
        .collect()
}

/// One path component for something nix-eval-jobs reported (an attribute or
/// an output name): never contains a separator and is never `.` or `..`.
pub fn path_component(s: &str) -> String {
    let s = s.replace(['/', '\0'], "_");
    match s.as_str() {
        "" | "." | ".." => "_".to_owned(),
        _ => s,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prep(attrs: &[&str]) -> Result<Vec<String>, AttrError> {
        prepare(&attrs.iter().map(|a| a.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn flake_style_prefixes_are_dropped() {
        assert_eq!(normalize("pkgs.hello"), "hello");
        assert_eq!(normalize("legacyPackages.x86_64-linux.hello"), "hello");
        assert_eq!(
            normalize("legacyPackages.aarch64-darwin.qt6Packages.qtbase"),
            "qt6Packages.qtbase"
        );
        assert_eq!(normalize("legacyPackages.x86_64-linux.pkgs.hello"), "hello");
        assert_eq!(normalize("hello"), "hello");
    }

    #[test]
    fn only_real_system_prefixes_are_dropped() {
        assert_eq!(normalize("legacyPackages.hello"), "legacyPackages.hello");
        assert_eq!(
            normalize("legacyPackages.x86_64-linux."),
            "legacyPackages.x86_64-linux."
        );
        assert_eq!(
            normalize("legacyPackages.X-Linux.hello"),
            "legacyPackages.X-Linux.hello"
        );
    }

    #[test]
    fn prepare_validates_and_dedupes_keeping_order() {
        assert_eq!(
            prep(&["b", "pkgs.a", "b", "a"]).unwrap(),
            ["b".to_owned(), "a".to_owned()]
        );
        assert_eq!(prep(&[]), Err(AttrError::None));
        for bad in ["", "1abc", "a b", "a;b", "a\"b", "a$(x)", "-a", ".a", "a/b"] {
            assert_eq!(
                prep(&[bad]),
                Err(AttrError::Invalid(bad.to_owned())),
                "{bad}"
            );
        }
        assert!(prep(&["_x.y-z.W1"]).is_ok());
    }

    #[test]
    fn systems() {
        assert!(validate_system("x86_64-linux").is_ok());
        assert!(validate_system("aarch64-darwin").is_ok());
        for bad in ["", "a\"b", "a b", "${x}", "a;b"] {
            assert!(validate_system(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn file_names() {
        assert_eq!(sanitize("qt6Packages.qtbase+x"), "qt6Packages.qtbase+x");
        assert_eq!(sanitize("a/b c"), "a-b-c");
        assert_eq!(sanitize(&"x".repeat(150)).len(), 100);
        assert_eq!(path_component("a/b"), "a_b");
        assert_eq!(path_component(".."), "_");
        assert_eq!(path_component(""), "_");
        assert_eq!(path_component("qt6.qtbase"), "qt6.qtbase");
    }
}
