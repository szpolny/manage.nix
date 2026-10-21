//! Verification of Nix store path signatures.
//!
//! Nix signs a path by signing its *fingerprint* with Ed25519:
//! `1;<path>;<nar hash>;<nar size>;<comma-separated references>`, where the
//! NAR hash is `sha256:` followed by Nix's base-32 encoding. Signatures and
//! public keys are both written as `<key name>:<base64>`.
//!
//! The agent checks the signature itself instead of asking Nix, because Nix
//! treats locally built ("ultimate") paths as trusted regardless of
//! signatures.

use anyhow::Context;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signature, VerifyingKey};

/// The metadata Nix signs for a store path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PathInfo {
    pub path: String,
    /// `sha256:<base-32>`, as printed by `nix-store --query --hash`.
    pub nar_hash: String,
    pub nar_size: u64,
    pub references: Vec<String>,
    pub signatures: Vec<String>,
}

impl PathInfo {
    pub fn fingerprint(&self) -> String {
        let mut references = self.references.clone();
        references.sort();
        format!(
            "1;{};{};{};{}",
            self.path,
            self.nar_hash,
            self.nar_size,
            references.join(",")
        )
    }
}

/// A set of public keys, any one of which may vouch for a path.
#[derive(Debug, Clone)]
pub struct TrustedKeys(Vec<(String, VerifyingKey)>);

impl TrustedKeys {
    pub fn parse(keys: &[String]) -> anyhow::Result<Self> {
        keys.iter()
            .map(|key| {
                let (name, bytes) = split_named(key)
                    .with_context(|| format!("invalid trusted public key {key:?}"))?;
                let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
                    anyhow::anyhow!("trusted public key {name:?} is not 32 bytes long")
                })?;
                let key = VerifyingKey::from_bytes(&bytes)
                    .with_context(|| format!("trusted public key {name:?} is not valid"))?;
                Ok((name.to_owned(), key))
            })
            .collect::<anyhow::Result<_>>()
            .map(Self)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Returns the name of a trusted key with a valid signature on `info`.
    pub fn verify(&self, info: &PathInfo) -> anyhow::Result<&str> {
        anyhow::ensure!(
            info.nar_hash.starts_with("sha256:"),
            "unexpected NAR hash format {:?}",
            info.nar_hash
        );
        let fingerprint = info.fingerprint();
        for sig in &info.signatures {
            let Ok((sig_name, sig_bytes)) = split_named(sig) else {
                continue;
            };
            let Ok(sig) = Signature::from_slice(&sig_bytes) else {
                continue;
            };
            for (name, key) in &self.0 {
                if name == sig_name && key.verify_strict(fingerprint.as_bytes(), &sig).is_ok() {
                    return Ok(name);
                }
            }
        }
        anyhow::bail!(
            "{} has no valid signature from a trusted key (signed by: {})",
            info.path,
            if info.signatures.is_empty() {
                "nobody".to_owned()
            } else {
                info.signatures
                    .iter()
                    .map(|s| s.split(':').next().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        )
    }
}

fn split_named(s: &str) -> anyhow::Result<(&str, Vec<u8>)> {
    let (name, data) = s
        .split_once(':')
        .filter(|(name, _)| !name.is_empty())
        .ok_or_else(|| anyhow::anyhow!("expected <name>:<base64>"))?;
    Ok((name, BASE64.decode(data.trim())?))
}

#[cfg(test)]
mod tests {
    use super::*;

    // A real path and signature from cache.nixos.org.
    const CACHE_KEY: &str = "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=";

    fn hello() -> PathInfo {
        PathInfo {
            path: "/nix/store/mklmiipy424axj6zgl1vnqg6a58mdz77-hello-2.12.3".into(),
            nar_hash: "sha256:05sa2cr63x7jw0cq7amq7jrdqc862cjl85j9mdd9adivbci9gzmn".into(),
            nar_size: 113096,
            references: vec!["/nix/store/fgrxz4hr8f71hkhsddbvk8nxadpg67bv-libiconv-115.100.1".into()],
            signatures: vec!["cache.nixos.org-1:7LYMNkbD0qH9hUCgpS4LYCX8giNXKBD9MiLbppLUs74GQEmT8i1YWbvEpz1cV6UEcY4O3/T1i/Rqq/GPxUuDDw==".into()],
        }
    }

    fn keys(keys: &[&str]) -> TrustedKeys {
        TrustedKeys::parse(&keys.iter().map(|k| k.to_string()).collect::<Vec<_>>()).unwrap()
    }

    #[test]
    fn accepts_a_valid_signature() {
        assert_eq!(
            keys(&[CACHE_KEY]).verify(&hello()).unwrap(),
            "cache.nixos.org-1"
        );
    }

    #[test]
    fn rejects_tampered_metadata() {
        let trusted = keys(&[CACHE_KEY]);
        let mut info = hello();
        info.nar_size += 1;
        assert!(trusted.verify(&info).is_err());
        let mut info = hello();
        info.references.clear();
        assert!(trusted.verify(&info).is_err());
    }

    #[test]
    fn rejects_unsigned_and_untrusted_paths() {
        let mut info = hello();
        info.signatures.clear();
        assert!(keys(&[CACHE_KEY]).verify(&info).is_err());

        // Same key bytes under another name: Nix matches keys by name.
        let renamed = CACHE_KEY.replace("cache.nixos.org-1", "other-1");
        assert!(keys(&[&renamed]).verify(&hello()).is_err());
        assert!(keys(&[]).verify(&hello()).is_err());
    }

    #[test]
    fn rejects_malformed_keys() {
        for key in ["", "nokey", ":AAAA", "name:not-base64!", "name:AAAA"] {
            assert!(TrustedKeys::parse(&[key.to_owned()]).is_err(), "{key:?}");
        }
    }
}
