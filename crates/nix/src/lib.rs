//! Nix store paths and signatures, shared by `mnix-agent` (which verifies
//! signatures before activating a system) and `mnix-server` (which signs the
//! systems it builds).

mod path_info;
mod signature;
mod store_path;

pub use path_info::query_path_info;
pub use signature::{PathInfo, SecretKey, TrustedKeys};
pub use store_path::{StorePath, STORE_DIR};
