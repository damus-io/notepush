//! Server keypair management for NIP-44 E2E encryption
//!
//! When NIP-44 encryption is enabled, the server needs a stable keypair to:
//! 1. Encrypt notification payloads to client device pubkeys
//! 2. Allow clients to verify/trust the server's identity
//!
//! The keypair can be loaded from the `SERVER_PRIVKEY` environment variable,
//! or generated fresh on startup (development mode only).

use nostr::Keys;

/// Server keypair for NIP-44 encryption operations
///
/// Holds both the secret key (for encryption) and public key (for client discovery).
/// The public key should be exposed via the `/server-pubkey` endpoint so clients
/// know which key to expect encrypted payloads from.
#[derive(Clone)]
pub struct ServerKeys {
    keys: Keys,
}

impl ServerKeys {
    /// Create ServerKeys from a hex-encoded secret key
    ///
    /// # Arguments
    /// * `secret_key_hex` - 64-character hex string of the 32-byte secret key
    ///
    /// # Returns
    /// `ServerKeys` on success, error if the hex is invalid or key is invalid
    pub fn from_secret_hex(secret_key_hex: &str) -> Result<Self, ServerKeysError> {
        let keys = Keys::parse(secret_key_hex).map_err(|_| ServerKeysError::InvalidSecretKey)?;
        Ok(Self { keys })
    }

    /// Generate a new random keypair
    ///
    /// WARNING: This should only be used for development/testing.
    /// In production, use `from_secret_hex` with a persistent key from env vars.
    /// A randomly generated key means clients can't verify server identity
    /// across restarts.
    pub fn generate() -> Self {
        let keys = Keys::generate();
        log::warn!(
            "Generated ephemeral server keypair. For production, set SERVER_PRIVKEY env var. \
             Current pubkey: {}",
            keys.public_key().to_hex()
        );
        Self { keys }
    }

    /// Load from environment or generate if not present
    ///
    /// Checks `SERVER_PRIVKEY` env var. If present, parses as hex secret key.
    /// If absent and `allow_generate` is true, generates a new keypair (dev mode).
    /// If absent and `allow_generate` is false, returns an error (production mode).
    pub fn from_env_or_generate(allow_generate: bool) -> Result<Self, ServerKeysError> {
        match std::env::var("SERVER_PRIVKEY") {
            Ok(hex) => Self::from_secret_hex(&hex),
            Err(_) if allow_generate => Ok(Self::generate()),
            Err(_) => Err(ServerKeysError::MissingEnvVar),
        }
    }

    /// Get the secret key for NIP-44 encryption
    ///
    /// Used as the sender secret key when encrypting notifications to device pubkeys.
    pub fn secret_key(&self) -> &nostr::SecretKey {
        // Keys::secret_key() returns Result, but we know it's valid since we constructed it
        self.keys
            .secret_key()
            .expect("ServerKeys always has a valid secret key")
    }

    /// Get the public key as a nostr PublicKey
    ///
    /// This is the key clients should know about - it's the "sender pubkey"
    /// they'll use when decrypting notifications from this server.
    pub fn public_key(&self) -> nostr::PublicKey {
        self.keys.public_key()
    }

    /// Get the public key as hex string for API responses
    pub fn public_key_hex(&self) -> String {
        self.keys.public_key().to_hex()
    }
}

/// Errors that can occur when loading or creating server keys
#[derive(Debug, Clone)]
pub enum ServerKeysError {
    /// The SERVER_PRIVKEY env var is not set and generation is not allowed
    MissingEnvVar,
    /// The input is not a valid secp256k1 secret key (invalid hex or invalid key bytes)
    InvalidSecretKey,
}

impl std::fmt::Display for ServerKeysError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::MissingEnvVar => write!(
                f,
                "SERVER_PRIVKEY environment variable is required when NIP44_ENABLED=true"
            ),
            Self::InvalidSecretKey => {
                write!(
                    f,
                    "SERVER_PRIVKEY must be a valid 64-character hex secp256k1 secret key"
                )
            }
        }
    }
}

impl std::error::Error for ServerKeysError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_from_secret_hex_valid() {
        // Known test vector: a valid secp256k1 secret key
        let secret_hex = "0000000000000000000000000000000000000000000000000000000000000001";
        let keys = ServerKeys::from_secret_hex(secret_hex);
        assert!(keys.is_ok());

        let keys = keys.unwrap();
        assert_eq!(keys.secret_key().to_secret_bytes()[31], 1);
    }

    #[test]
    fn test_from_secret_hex_invalid_input() {
        // Keys::parse handles both hex decoding and key validation,
        // so invalid hex returns InvalidSecretKey
        let result = ServerKeys::from_secret_hex("not-valid-hex");
        assert!(matches!(result, Err(ServerKeysError::InvalidSecretKey)));
    }

    #[test]
    fn test_from_secret_hex_invalid_key() {
        // All zeros is not a valid secret key
        let secret_hex = "0000000000000000000000000000000000000000000000000000000000000000";
        let result = ServerKeys::from_secret_hex(secret_hex);
        assert!(matches!(result, Err(ServerKeysError::InvalidSecretKey)));
    }

    #[test]
    fn test_generate_produces_valid_keys() {
        let keys = ServerKeys::generate();
        // Should be able to get both keys without panic
        let _ = keys.secret_key();
        let _ = keys.public_key();
        let hex = keys.public_key_hex();
        assert_eq!(hex.len(), 64);
    }
}
