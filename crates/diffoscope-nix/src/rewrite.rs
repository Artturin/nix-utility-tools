//! Byte-level masking of Nix store hashes. Every replacement has the same
//! length as the text it replaces, so offsets in binaries don't move.

use std::borrow::Cow;
use std::sync::LazyLock;

use regex::bytes::{Captures, Regex};

/// The nixbase32 alphabet (no e, o, u, t), so a placeholder of `e`s can never
/// match again.
const HASH: &str = "[0-9a-df-np-sv-z]{32}";
const HASH_UPPER: &str = "[0-9A-DF-NP-SV-Z]{32}";

const PLACEHOLDER: &[u8] = b"eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";
const PLACEHOLDER_UPPER: &[u8] = b"EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEE";

fn regex(pattern: String) -> Regex {
    Regex::new(&format!("(?-u){pattern}")).expect("valid regex")
}

static STORE: LazyLock<Regex> = LazyLock::new(|| regex(format!("/nix/store/{HASH}")));

// Some builds embed the path upper-cased (e.g. "/nix/store/RJXGV8QS...-qtbase"
// in the assert strings of a Qt debug build). The placeholder is upper-cased
// too, so a case change between the builds would still show up.
static STORE_UPPER: LazyLock<Regex> = LazyLock::new(|| regex(format!("/nix/store/{HASH_UPPER}")));

// UTF-16LE string literals (QString/char16_t data in .rodata): the prefix with
// a NUL after every character, then the hash, one character per 2 bytes.
static STORE_UTF16: LazyLock<Regex> = LazyLock::new(|| {
    regex(format!(
        r"(/\x00n\x00i\x00x\x00/\x00s\x00t\x00o\x00r\x00e\x00/\x00)(?:{}\x00){{32}}",
        "[0-9a-df-np-sv-z]"
    ))
});

// Relative references to a sibling store path, with no /nix/store prefix,
// e.g. "${CMAKE_CURRENT_LIST_DIR}/../../../../<hash>-name" in *Config.cmake.
// A whole path component of 32 nixbase32 characters followed by "-name".
static RELATIVE: LazyLock<Regex> = LazyLock::new(|| regex(format!("(/){HASH}(-[A-Za-z0-9])")));

/// Replace every store hash in `data`. Borrowed when nothing matched.
pub fn rewrite(data: &[u8]) -> Cow<'_, [u8]> {
    let mut replacement = b"/nix/store/".to_vec();
    replacement.extend_from_slice(PLACEHOLDER);
    let mut replacement_upper = b"/nix/store/".to_vec();
    replacement_upper.extend_from_slice(PLACEHOLDER_UPPER);

    let mut out = Cow::Borrowed(data);
    replace(&mut out, &STORE, replacement.as_slice());
    replace(&mut out, &STORE_UPPER, replacement_upper.as_slice());
    replace(&mut out, &STORE_UTF16, |caps: &Captures| {
        let mut v = caps[1].to_vec();
        v.extend(PLACEHOLDER.iter().flat_map(|&c| [c, 0]));
        v
    });
    replace(&mut out, &RELATIVE, |caps: &Captures| {
        let mut v = caps[1].to_vec();
        v.extend_from_slice(PLACEHOLDER);
        v.extend_from_slice(&caps[2]);
        v
    });
    out
}

fn replace<R: regex::bytes::Replacer>(data: &mut Cow<'_, [u8]>, re: &Regex, rep: R) {
    if let Cow::Owned(v) = re.replace_all(data, rep) {
        *data = Cow::Owned(v);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const H1: &str = "rjxgv8qsq5kdyz0zk1fqj6d9a7d0n3sv";
    const H2: &str = "0123456789abcdfghijklmnpqrsvwxyz";

    fn run(input: &[u8]) -> Vec<u8> {
        rewrite(input).into_owned()
    }

    #[test]
    fn masks_store_paths() {
        let a = run(format!("rpath=/nix/store/{H1}-qtbase/lib:/nix/store/{H2}-glibc").as_bytes());
        assert_eq!(
            a,
            b"rpath=/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-qtbase/lib:/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-glibc"
        );
    }

    #[test]
    fn unchanged_input_is_borrowed() {
        assert!(matches!(
            rewrite(b"/nix/store/short-name"),
            Cow::Borrowed(_)
        ));
    }

    #[test]
    fn masks_upper_case() {
        let input = format!("/nix/store/{}-qtbase", H1.to_uppercase());
        assert_eq!(
            run(input.as_bytes()),
            b"/nix/store/EEEEEEEEEEEEEEEEEEEEEEEEEEEEEEEE-qtbase"
        );
    }

    #[test]
    fn masks_utf16() {
        let widen = |s: &str| s.bytes().flat_map(|c| [c, 0]).collect::<Vec<u8>>();
        let input = widen(&format!("/nix/store/{H1}-x"));
        assert_eq!(
            run(&input),
            widen("/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-x")
        );
    }

    #[test]
    fn masks_relative_references() {
        let input = format!("${{DIR}}/../../{H1}-qtbase/lib ../{H2}-glibc");
        assert_eq!(
            run(input.as_bytes()),
            b"${DIR}/../../eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-qtbase/lib ../eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-glibc"
        );
    }

    #[test]
    fn relative_needs_a_name_and_a_leading_slash() {
        for input in [format!("/{H1}"), format!("/{H1}-"), format!("{H1}-name")] {
            assert_eq!(run(input.as_bytes()), input.as_bytes());
        }
    }
}
