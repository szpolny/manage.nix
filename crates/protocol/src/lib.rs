//! Message types shared by `mnix-agent` and `mnix-server`.
//!
//! Protocol v1 messages are added in M2.

use serde::{Deserialize, Serialize};

/// The protocol version this build speaks.
pub const PROTOCOL_VERSION: ProtocolVersion = ProtocolVersion(1);

/// A protocol version, sent by both sides when a session starts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ProtocolVersion(pub u32);

impl std::fmt::Display for ProtocolVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "v{}", self.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_serializes_as_a_bare_number() {
        assert_eq!(serde_json::to_string(&PROTOCOL_VERSION).unwrap(), "1");
        assert_eq!(PROTOCOL_VERSION.to_string(), "v1");
    }
}
