//! Validated Nix store paths.

use std::fmt;
use std::path::Path;

pub const STORE_DIR: &str = "/nix/store";

/// Characters Nix uses in store path hashes (its base-32 alphabet).
const HASH_CHARS: &str = "0123456789abcdfghijklmnpqrsvwxyz";
const HASH_LEN: usize = 32;

/// An absolute top-level path in `/nix/store`, such as
/// `/nix/store/<hash>-nixos-system-web-25.11`. Subpaths are not allowed.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct StorePath(String);

impl StorePath {
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let invalid = || anyhow::anyhow!("not a top-level Nix store path: {s:?}");
        let base = s
            .strip_prefix(STORE_DIR)
            .and_then(|rest| rest.strip_prefix('/'))
            .ok_or_else(invalid)?;
        let (hash, name) = base.split_at_checked(HASH_LEN).ok_or_else(invalid)?;
        let name = name.strip_prefix('-').ok_or_else(invalid)?;
        let hash_ok = hash.chars().all(|c| HASH_CHARS.contains(c));
        let name_ok = !name.is_empty()
            && !name.starts_with('.')
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-._?=".contains(c));
        if hash_ok && name_ok {
            Ok(Self(s.to_owned()))
        } else {
            Err(invalid())
        }
    }

    /// Resolves a symlink such as `/run/current-system` to the store path it
    /// points at.
    pub fn resolve(link: &Path) -> anyhow::Result<Self> {
        let target = std::fs::canonicalize(link)
            .map_err(|e| anyhow::anyhow!("resolving {}: {e}", link.display()))?;
        let target = target
            .to_str()
            .ok_or_else(|| anyhow::anyhow!("{} is not valid UTF-8", target.display()))?;
        Self::parse(target)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn as_path(&self) -> &Path {
        Path::new(&self.0)
    }
}

impl fmt::Display for StorePath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_top_level_store_paths() {
        let p = "/nix/store/mklmiipy424axj6zgl1vnqg6a58mdz77-hello-2.12.3";
        assert_eq!(StorePath::parse(p).unwrap().as_str(), p);
    }

    #[test]
    fn rejects_everything_else() {
        for p in [
            "",
            "/nix/store",
            "/nix/store/",
            "nix/store/mklmiipy424axj6zgl1vnqg6a58mdz77-hello",
            "/nix/storex/mklmiipy424axj6zgl1vnqg6a58mdz77-hello",
            "/nix/store/mklmiipy424axj6zgl1vnqg6a58mdz77",
            "/nix/store/mklmiipy424axj6zgl1vnqg6a58mdz77-",
            "/nix/store/mklmiipy424axj6zgl1vnqg6a58mdz7e-hello",
            "/nix/store/mklmiipy424axj6zgl1vnqg6a58mdz77-hello/bin",
            "/nix/store/mklmiipy424axj6zgl1vnqg6a58mdz77-..",
            "/nix/store/mklmiipy424axj6zgl1vnqg6a58mdz77-a b",
        ] {
            assert!(StorePath::parse(p).is_err(), "{p:?} should be rejected");
        }
    }
}
