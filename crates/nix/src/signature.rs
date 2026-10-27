//! Signing and verifying Nix store path signatures.
//!
//! Nix signs a path by signing its *fingerprint* with Ed25519:
//! `1;<path>;<nar hash>;<nar size>;<comma-separated references>`, where the
//! NAR hash is `sha256:` followed by Nix's base-32 encoding. Signatures and
//! public keys are both written as `<key name>:<base64>`; secret keys as
//! `<key name>:<base64 of the 32-byte seed and the 32-byte public key>`, the
//! format of `nix key generate-secret`.
//!
//! The agent checks signatures itself instead of asking Nix, because Nix
//! treats locally built ("ultimate") paths as trusted regardless of
//! signatures.

use anyhow::Context;
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, VerifyingKey};

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

    /// Returns the name of a trusted key with a valid signature on `info`,
    /// looking at the signatures Nix has for the path and at `extra`
    /// signatures obtained some other way.
    pub fn verify(&self, info: &PathInfo, extra: &[String]) -> anyhow::Result<&str> {
        anyhow::ensure!(
            info.nar_hash.starts_with("sha256:"),
            "unexpected NAR hash format {:?}",
            info.nar_hash
        );
        let fingerprint = info.fingerprint();
        for sig in info.signatures.iter().chain(extra) {
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
            if info.signatures.is_empty() && extra.is_empty() {
                "nobody".to_owned()
            } else {
                info.signatures
                    .iter()
                    .chain(extra)
                    .map(|s| s.split(':').next().unwrap_or_default())
                    .collect::<Vec<_>>()
                    .join(", ")
            }
        )
    }
}

/// A Nix signing key.
#[derive(Clone)]
pub struct SecretKey {
    name: String,
    key: SigningKey,
}

impl std::fmt::Debug for SecretKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SecretKey")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl SecretKey {
    /// A new random key called `name` (for example `manage-nix-1`).
    pub fn generate(name: &str) -> anyhow::Result<Self> {
        anyhow::ensure!(
            !name.is_empty() && !name.contains(':') && !name.contains(char::is_whitespace),
            "invalid key name {name:?}"
        );
        let mut seed = [0u8; 32];
        getrandom::fill(&mut seed).map_err(|e| anyhow::anyhow!("generating a key: {e}"))?;
        Ok(Self {
            name: name.to_owned(),
            key: SigningKey::from_bytes(&seed),
        })
    }

    /// Parses `<name>:<base64>`, as written by `nix key generate-secret`.
    pub fn parse(s: &str) -> anyhow::Result<Self> {
        let (name, bytes) = split_named(s.trim()).context("invalid secret key")?;
        let bytes: [u8; 64] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("secret key {name:?} is not 64 bytes long"))?;
        let key = SigningKey::from_keypair_bytes(&bytes)
            .map_err(|_| anyhow::anyhow!("secret key {name:?} does not match its public key"))?;
        Ok(Self {
            name: name.to_owned(),
            key,
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// The secret key in Nix's format. Keep it secret.
    pub fn to_secret_string(&self) -> String {
        format!(
            "{}:{}",
            self.name,
            BASE64.encode(self.key.to_keypair_bytes())
        )
    }

    /// The public key in Nix's format, for `trusted-public-keys` and
    /// `services.manage-nix.agent.trustedPublicKeys`.
    pub fn public_key(&self) -> String {
        format!(
            "{}:{}",
            self.name,
            BASE64.encode(self.key.verifying_key().as_bytes())
        )
    }

    /// Signs a path, returning a signature in Nix's format.
    pub fn sign(&self, info: &PathInfo) -> String {
        let sig = self.key.sign(info.fingerprint().as_bytes());
        format!("{}:{}", self.name, BASE64.encode(sig.to_bytes()))
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
            keys(&[CACHE_KEY]).verify(&hello(), &[]).unwrap(),
            "cache.nixos.org-1"
        );
    }

    #[test]
    fn rejects_tampered_metadata() {
        let trusted = keys(&[CACHE_KEY]);
        let mut info = hello();
        info.nar_size += 1;
        assert!(trusted.verify(&info, &[]).is_err());
        let mut info = hello();
        info.references.clear();
        assert!(trusted.verify(&info, &[]).is_err());
    }

    #[test]
    fn rejects_unsigned_and_untrusted_paths() {
        let mut info = hello();
        info.signatures.clear();
        assert!(keys(&[CACHE_KEY]).verify(&info, &[]).is_err());

        // Same key bytes under another name: Nix matches keys by name.
        let renamed = CACHE_KEY.replace("cache.nixos.org-1", "other-1");
        assert!(keys(&[&renamed]).verify(&hello(), &[]).is_err());
        assert!(keys(&[]).verify(&hello(), &[]).is_err());
    }

    #[test]
    fn accepts_signatures_supplied_separately() {
        let mut info = hello();
        let sigs = std::mem::take(&mut info.signatures);
        let trusted = keys(&[CACHE_KEY]);
        assert!(trusted.verify(&info, &[]).is_err());
        assert_eq!(trusted.verify(&info, &sigs).unwrap(), "cache.nixos.org-1");
    }

    #[test]
    fn signs_like_nix() {
        // The VM tests' key (nix/tests/keys), generated with
        // `nix key generate-secret --key-name mnix-test-1`.
        let secret = SecretKey::parse("mnix-test-1:4vylCMWVCPZxLePAJAHNRbKoc8n7Zw7FUvwV0uex8nErjUeRRs35mpJxkDtn20HS9blgzLy2o2ntiExGhKE6wA==").unwrap();
        let public = "mnix-test-1:K41HkUbN+ZqScZA7Z9tB0vW5YMy8tqNp7YhMRoShOsA=";
        assert_eq!(secret.name(), "mnix-test-1");
        assert_eq!(secret.public_key(), public);
        assert_eq!(
            SecretKey::parse(&secret.to_secret_string())
                .unwrap()
                .public_key(),
            public
        );
        assert!(!format!("{secret:?}").contains(&secret.to_secret_string()));

        let mut info = hello();
        info.signatures = vec![secret.sign(&info)];
        assert_eq!(keys(&[public]).verify(&info, &[]).unwrap(), "mnix-test-1");
        assert!(keys(&[CACHE_KEY]).verify(&info, &[]).is_err());
    }

    #[test]
    fn generates_distinct_keys() {
        let a = SecretKey::generate("manage-nix-1").unwrap();
        let b = SecretKey::generate("manage-nix-1").unwrap();
        assert_ne!(a.public_key(), b.public_key());
        assert!(a.public_key().starts_with("manage-nix-1:"));
        assert!(SecretKey::generate("bad:name").is_err());
        assert!(SecretKey::parse(&a.public_key()).is_err());
    }

    #[test]
    fn rejects_malformed_keys() {
        for key in ["", "nokey", ":AAAA", "name:not-base64!", "name:AAAA"] {
            assert!(TrustedKeys::parse(&[key.to_owned()]).is_err(), "{key:?}");
        }
    }
}
