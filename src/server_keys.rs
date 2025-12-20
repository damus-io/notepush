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
        let env_value = std::env::var("SERVER_PRIVKEY").ok();
        Self::from_env_value(env_value.as_deref(), allow_generate)
    }

    /// Internal helper for testable env gating logic
    ///
    /// Takes the env value directly (None if unset) to enable unit testing
    /// without actually modifying environment variables.
    fn from_env_value(
        server_privkey: Option<&str>,
        allow_generate: bool,
    ) -> Result<Self, ServerKeysError> {
        match server_privkey {
            Some(hex) => Self::from_secret_hex(hex),
            None if allow_generate => Ok(Self::generate()),
            None => Err(ServerKeysError::MissingEnvVar),
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

    #[test]
    fn test_nip44_encrypt_decrypt_roundtrip() {
        // Server encrypts notification to client device pubkey
        let server_keys = ServerKeys::generate();
        let client_keys = Keys::generate();

        let plaintext = r#"{"id":"abc","content":"Hello world"}"#;

        // Server encrypts to client's public key
        let ciphertext = nostr::nips::nip44::encrypt(
            server_keys.secret_key(),
            &client_keys.public_key(),
            plaintext,
            nostr::nips::nip44::Version::V2,
        )
        .expect("encryption should succeed");

        // Ciphertext should be base64-encoded and different from plaintext
        assert!(!ciphertext.is_empty());
        assert_ne!(ciphertext, plaintext);

        // Client decrypts using server's public key
        let decrypted = nostr::nips::nip44::decrypt(
            client_keys.secret_key().expect("test key has secret"),
            &server_keys.public_key(),
            &ciphertext,
        )
        .expect("decryption should succeed");

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn test_nip44_wrong_key_fails_decryption() {
        // Verify that wrong keys cannot decrypt
        let server_keys = ServerKeys::generate();
        let client_keys = Keys::generate();
        let wrong_keys = Keys::generate();

        let plaintext = "secret message";

        let ciphertext = nostr::nips::nip44::encrypt(
            server_keys.secret_key(),
            &client_keys.public_key(),
            plaintext,
            nostr::nips::nip44::Version::V2,
        )
        .expect("encryption should succeed");

        // Attempting to decrypt with wrong sender pubkey should fail
        let result = nostr::nips::nip44::decrypt(
            client_keys.secret_key().expect("test key has secret"),
            &wrong_keys.public_key(), // Wrong server pubkey
            &ciphertext,
        );

        // Decryption with wrong key should fail
        assert!(result.is_err());
    }

    #[test]
    fn test_nip44_payload_format() {
        // Verify the ciphertext format is valid NIP-44 (base64 with version byte)
        let server_keys = ServerKeys::generate();
        let client_keys = Keys::generate();

        let ciphertext = nostr::nips::nip44::encrypt(
            server_keys.secret_key(),
            &client_keys.public_key(),
            "test",
            nostr::nips::nip44::Version::V2,
        )
        .expect("encryption should succeed");

        // NIP-44 ciphertext should be base64-encoded
        // It should decode successfully
        use base64::Engine;
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&ciphertext)
            .expect("ciphertext should be valid base64");

        // First byte should be version 2
        assert_eq!(decoded[0], 2, "NIP-44 version byte should be 2");
    }

    // Tests for env gating logic (NIP44_ENABLED + SERVER_PRIVKEY + NIP44_ALLOW_EPHEMERAL)

    #[test]
    fn test_from_env_value_with_valid_key() {
        // When SERVER_PRIVKEY is set to valid hex, should load successfully
        let valid_key = "0000000000000000000000000000000000000000000000000000000000000001";
        let result = ServerKeys::from_env_value(Some(valid_key), false);
        assert!(result.is_ok());

        let keys = result.unwrap();
        assert_eq!(keys.secret_key().to_secret_bytes()[31], 1);
    }

    #[test]
    fn test_from_env_value_with_invalid_key() {
        // When SERVER_PRIVKEY is set but invalid, should return error
        let result = ServerKeys::from_env_value(Some("invalid-hex"), false);
        assert!(matches!(result, Err(ServerKeysError::InvalidSecretKey)));
    }

    #[test]
    fn test_from_env_value_missing_without_allow_generate() {
        // Production mode: SERVER_PRIVKEY missing + allow_generate=false → error
        // This is the critical security check: NIP44_ENABLED=true requires SERVER_PRIVKEY
        let result = ServerKeys::from_env_value(None, false);
        assert!(matches!(result, Err(ServerKeysError::MissingEnvVar)));
    }

    #[test]
    fn test_from_env_value_missing_with_allow_generate() {
        // Dev mode: SERVER_PRIVKEY missing + allow_generate=true → generates ephemeral
        // This is only used when NIP44_ALLOW_EPHEMERAL=true
        let result = ServerKeys::from_env_value(None, true);
        assert!(result.is_ok());

        // Verify generated keys are valid
        let keys = result.unwrap();
        let _ = keys.secret_key();
        let _ = keys.public_key();
    }

    #[test]
    fn test_from_env_value_prefers_env_over_generate() {
        // When SERVER_PRIVKEY is set, should use it even if allow_generate=true
        let valid_key = "0000000000000000000000000000000000000000000000000000000000000001";
        let result = ServerKeys::from_env_value(Some(valid_key), true);
        assert!(result.is_ok());

        // Verify it loaded from env (key byte 31 == 1), not generated (random)
        let keys = result.unwrap();
        assert_eq!(keys.secret_key().to_secret_bytes()[31], 1);
    }
}
