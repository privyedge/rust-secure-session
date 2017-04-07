//! Sessions and session management utilities

use bincode::{self, Infinite};
use chrono::{DateTime, UTC};
use crypto::aead::{AeadEncryptor, AeadDecryptor};
use crypto::chacha20poly1305::ChaCha20Poly1305;
use crypto::scrypt::{scrypt, ScryptParams};
use ring::rand::SystemRandom;
use std::collections::HashMap;
use typemap;

use error::SessionError;

const SCRYPT_SALT: &'static [u8; 31] = b"rust-secure-session-scrypt-salt";


/// Container for serializing and deserializing the session when sending it to and receiving it
/// from a client.
#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq)]
pub struct SessionTransport {
    /// Optional UTC timestamp for when the session expires.
    pub expires: Option<DateTime<UTC>>,
    /// The session that was deserialized or will be serialized.
    pub session: Session,
}


/// Persistent session passed to client as a cookie.
///
/// ```
/// use secure_session::session::Session;
///
/// let mut session = Session::new();
/// assert_eq!(session.insert_bytes("foo", b"bar".to_vec()), None);
/// assert_eq!(session.get_bytes("foo"), Some(&b"bar".to_vec()));
///
/// session.clear();
/// assert_eq!(session.get_bytes("foo"), None);
/// ```
#[derive(Clone, Serialize, Deserialize, Debug, Eq, PartialEq)]
pub struct Session {
    bytes: HashMap<String, Vec<u8>>,
}

impl Session {
    /// Create an empty session.
    pub fn new() -> Self {
        Session { bytes: HashMap::new() }
    }

    /// Store bytes for the given key.
    ///
    /// ```
    /// use secure_session::session::Session;
    ///
    /// let mut session = Session::new();
    /// assert_eq!(session.insert_bytes("foo", b"bar".to_vec()), None);
    /// assert_eq!(session.get_bytes("foo"), Some(&b"bar".to_vec()));
    /// ```
    pub fn get_bytes(&self, key: &str) -> Option<&Vec<u8>> {
        self.bytes.get(key)
    }

    /// Retrieve bytes for the given key.
    /// If the key was occupied, return the previous value.
    ///
    /// ```
    /// use secure_session::session::Session;
    ///
    /// let mut session = Session::new();
    /// assert_eq!(session.insert_bytes("foo", b"bar".to_vec()), None);
    /// ```
    pub fn insert_bytes(&mut self, key: &str, bytes: Vec<u8>) -> Option<Vec<u8>> {
        self.bytes.insert(key.to_string(), bytes)
    }

    /// Remove bytes stored at the given key.
    ///
    /// ```
    /// use secure_session::session::Session;
    ///
    /// let mut session = Session::new();
    /// assert_eq!(session.remove_bytes("foo"), None);
    ///
    /// let _ = session.insert_bytes("foo", b"bar".to_vec());
    /// assert_eq!(session.remove_bytes("foo"), Some(b"bar".to_vec()));
    /// ```
    pub fn remove_bytes(&mut self, key: &str) -> Option<Vec<u8>> {
        self.bytes.remove(key)
    }

    /// Check whether the session contains bytes stored at the given key.
    ///
    /// ```
    /// use secure_session::session::Session;
    ///
    /// let mut session = Session::new();
    /// let _ = session.insert_bytes("foo", b"bar".to_vec());
    /// assert!(session.contains_key("foo"));
    /// ```
    pub fn contains_key(&self, key: &str) -> bool {
        self.bytes.contains_key(key)
    }

    /// Clears all the values from the session.
    ///
    /// ```
    /// use secure_session::session::Session;
    ///
    /// let mut session = Session::new();
    /// let _ = session.insert_bytes("foo", b"bar".to_vec());
    /// let _ = session.insert_bytes("wat", b"lol".to_vec());
    ///
    /// session.clear();
    ///
    /// assert!(!session.contains_key("foo"));
    /// assert!(!session.contains_key("wat"));
    /// ```
    pub fn clear(&mut self) {
        self.bytes.clear()
    }
}

impl typemap::Key for Session {
    type Value = Session;
}

/// Base trait that provides session management.
pub trait SessionManager: Send + Sync {
    /// Using `scrypt` with params `n=12`, `r=8`, `p=1`, generate the key material used for the
    /// underlying crypto functions.
    ///
    /// # Panics
    /// This function may panic if the underlying crypto library fails catastrophically.
    fn from_password(password: &[u8]) -> Self;

    /// Given a slice of bytes perform the following options to convert it into a `SessionTransport`:
    ///
    ///   * Decrypt (optional)
    ///   * Verify signature / MAC
    ///   * Parse / deserialize into a `SessionTransport` struct
    fn deserialize(&self, bytes: &[u8]) -> Result<SessionTransport, SessionError>;

    /// Given a session perform the following options to convert a `SessionTransport ` into bytes:
    ///
    ///   * Encrypt (optional)
    ///   * Sign / MAC
    ///   * Encode / serialize into bytes
    fn serialize(&self, session: &SessionTransport) -> Result<Vec<u8>, SessionError>;

    /// Whether or not the sessions are encrypted.
    fn is_encrypted(&self) -> bool;
}


/// Uses the ChaCha20Poly1305 AEAD to provide signed, encrypted sessions.
pub struct ChaCha20Poly1305SessionManager {
    rng: SystemRandom,
    aead_key: [u8; 32],
}

impl ChaCha20Poly1305SessionManager {
    /// Using a saved key, generate a `ChaCha20Poly1305SessionManager`.
    pub fn from_key(aead_key: [u8; 32]) -> Self {
        ChaCha20Poly1305SessionManager {
            rng: SystemRandom::new(),
            aead_key: aead_key,
        }
    }

    fn random_bytes(&self, buf: &mut [u8]) -> Result<(), SessionError> {
        self.rng
            .fill(buf)
            .map_err(|_| {
                warn!("Failed to get random bytes");
                SessionError::InternalError
            })
    }

    fn aead(&self, nonce: &[u8; 8]) -> ChaCha20Poly1305 {
        ChaCha20Poly1305::new(&self.aead_key, nonce, &[])
    }
}

impl SessionManager for ChaCha20Poly1305SessionManager {
    fn from_password(password: &[u8]) -> Self {
        let params = if cfg!(test) {
            // scrypt is *slow*, so use these params for testing
            ScryptParams::new(1, 8, 1)
        } else {
            ScryptParams::new(12, 8, 1)
        };

        let mut aead_key = [0; 32];
        info!("Generating key material. This may take some time.");
        scrypt(password, SCRYPT_SALT, &params, &mut aead_key);
        info!("Key material generated.");

        ChaCha20Poly1305SessionManager::from_key(aead_key)
    }

    fn deserialize(&self, bytes: &[u8]) -> Result<SessionTransport, SessionError> {
        if bytes.len() <= 40 {
            return Err(SessionError::ValidationError);
        }

        let mut ciphertext = vec![0; bytes.len() - 24];
        let mut plaintext = vec![0; bytes.len() - 24];
        let mut tag = [0; 16];
        let mut nonce = [0; 8];

        for i in 0..8 {
            nonce[i] = bytes[i];
        }
        for i in 0..16 {
            tag[i] = bytes[i + 8];
        }
        for i in 0..(bytes.len() - 24) {
            ciphertext[i] = bytes[i + 24];
        }

        let mut aead = self.aead(&nonce);
        if !aead.decrypt(&ciphertext, &mut plaintext, &tag) {
            info!("Failed to decrypt session");
            return Err(SessionError::ValidationError);
        }

        Ok(bincode::deserialize(&plaintext[16..plaintext.len()]).unwrap()) // TODO unwrap
    }

    fn serialize(&self, session: &SessionTransport) -> Result<Vec<u8>, SessionError> {
        let mut nonce = [0; 8];
        self.random_bytes(&mut nonce)?;

        let session_bytes = bincode::serialize(&session, Infinite).unwrap(); // TODO unwrap
        let mut padding = [0; 16];
        self.random_bytes(&mut padding)?;

        let mut plaintext = vec![0; session_bytes.len() + 16];

        for i in 0..16 {
            plaintext[i] = padding[i];
        }
        for i in 0..session_bytes.len() {
            plaintext[i + 16] = session_bytes[i];
        }

        let mut ciphertext = vec![0; plaintext.len()];
        let mut tag = [0; 16];
        let mut aead = self.aead(&nonce);

        aead.encrypt(&plaintext, &mut ciphertext, &mut tag);

        let mut transport = vec![0; ciphertext.len() + 24];

        for i in 0..8 {
            transport[i] = nonce[i];
        }
        for i in 0..16 {
            transport[i + 8] = tag[i];
        }
        for i in 0..ciphertext.len() {
            transport[i + 24] = ciphertext[i];
        }

        Ok(transport)
    }

    /// Whether or not the sessions are encrypted.
    ///
    /// ```
    /// use secure_session::session::{ChaCha20Poly1305SessionManager, SessionManager};
    ///
    /// let manager = ChaCha20Poly1305SessionManager::from_key(*b"01234567012345670123456701234567");
    /// assert!(manager.is_encrypted());
    /// ```
    fn is_encrypted(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {

    macro_rules! test_cases {
        ($strct: ident, $md: ident) => {
            mod $md  {
                use $crate::session::{$strct, SessionManager, SessionTransport, Session};

                const KEY: [u8; 32] = *b"01234567012345670123456701234567";

                #[test]
                fn serde_happy_path() {
                    let manager = $strct::from_key(KEY);
                    let mut session = Session::new();
                    let key = "lol".to_string();
                    let value = b"wat".to_vec();
                    assert!(session.insert_bytes(&key, value.clone()).is_none());

                    let transport = SessionTransport { expires: None, session: session };

                    let bytes = manager.serialize(&transport).expect("couldn't serialize");
                    let parsed_transport = manager.deserialize(&bytes).expect("couldn't deserialize");
                    assert_eq!(parsed_transport, transport);
                    assert_eq!(parsed_transport.session.get_bytes(&key), Some(&value));
                }
            }
        }
    }

    test_cases!(ChaCha20Poly1305SessionManager, chacha20poly1305);
}
