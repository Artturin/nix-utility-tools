//! Nix store path syntax shared by the tools that mask store hashes.

/// One character of the nixbase32 alphabet (no `e`, `o`, `u`, `t`), as a
/// regex character class.
pub const HASH_CHAR: &str = "[0-9a-df-np-sv-z]";

/// The same alphabet upper-cased, for builds that embed paths upper-cased.
pub const HASH_CHAR_UPPER: &str = "[0-9A-DF-NP-SV-Z]";

/// Length of the hash part of a store path name.
pub const HASH_LEN: usize = 32;

/// A whole store hash as a regex, e.g. for `/nix/store/{hash_pattern()}-`.
pub fn hash_pattern() -> String {
    format!("{HASH_CHAR}{{{HASH_LEN}}}")
}

/// A placeholder of `e`s the length of a hash. `e` is not in the alphabet,
/// so a masked hash can never match again.
pub const PLACEHOLDER: &str = "eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholder_has_the_hash_length() {
        assert_eq!(PLACEHOLDER.len(), HASH_LEN);
        assert_eq!(hash_pattern(), "[0-9a-df-np-sv-z]{32}");
    }
}
