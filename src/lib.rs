#![cfg_attr(docsrs, feature(doc_cfg))]
//! A minimalist library to interact with encrypted JSON keystores as per the
//! [Web3 Secret Storage Definition](https://github.com/ethereum/wiki/wiki/Web3-Secret-Storage-Definition).
use aes::{
    cipher::{NewStreamCipher, SyncStreamCipher},
    Aes128,
};
use ctr::Ctr128;
use digest::Digest;
use hmac::Hmac;
use pbkdf2::pbkdf2;
use scrypt::{scrypt, Params as ScryptParams};
use sha2::Sha256;
use sha3::Keccak256;

use rand::{CryptoRng, Rng};
use std::{
    fs::File,
    io::{Read, Write},
    path::Path,
};
use uuid::Uuid;

mod error;
mod keystore;

pub use error::KeystoreError;
pub use keystore::{CipherparamsJson, CryptoJson, EthKeystore, KdfType, KdfparamsType};

const DEFAULT_CIPHER: &str = "aes-128-ctr";
const DEFAULT_KEY_SIZE: usize = 32usize;
const DEFAULT_IV_SIZE: usize = 16usize;
const DEFAULT_KDF_PARAMS_DKLEN: u8 = 32u8;
const DEFAULT_KDF_PARAMS_LOG_N: u8 = 13u8;
const DEFAULT_KDF_PARAMS_R: u32 = 8u32;
const DEFAULT_KDF_PARAMS_P: u32 = 1u32;

/// Creates a new JSON keystore using the [Scrypt](https://tools.ietf.org/html/rfc7914.html)
/// key derivation function. The keystore is encrypted by a key derived from the provided `password`
/// and stored in the provided directory.
///
/// # Example
///
/// ```no_run
/// use eth_keystore::new;
/// use std::path::Path;
///
/// # async fn foobar() -> Result<(), Box<dyn std::error::Error>> {
/// let dir = Path::new("./keys");
/// let mut rng = rand::thread_rng();
/// let (private_key, uuid) = new(&dir, &mut rng, "password_to_keystore")?;
/// # Ok(())
/// # }
/// ```
pub fn new<P, R, S>(dir: P, rng: &mut R, password: S) -> Result<(Vec<u8>, String), KeystoreError>
where
    P: AsRef<Path>,
    R: Rng + CryptoRng,
    S: AsRef<[u8]>,
{
    // Generate a random private key.
    let mut pk = vec![0u8; DEFAULT_KEY_SIZE];
    rng.fill_bytes(pk.as_mut_slice());

    let uuid = encrypt_key(dir, rng, pk.clone(), password)?;
    Ok((pk, uuid))
}

/// Decrypts an encrypted JSON keystore at the provided `path` using the provided `password`.
/// Decryption supports the [Scrypt](https://tools.ietf.org/html/rfc7914.html) and
/// [PBKDF2](https://ietf.org/rfc/rfc2898.txt) key derivation functions.
///
/// # Example
///
/// ```no_run
/// use eth_keystore::decrypt_key;
/// use std::path::Path;
///
/// # async fn foobar() -> Result<(), Box<dyn std::error::Error>> {
/// let keypath = Path::new("./keys/my-key");
/// let private_key = decrypt_key(&keypath, "password_to_keystore")?;
/// # Ok(())
/// # }
/// ```
pub fn decrypt_key<P, S>(path: P, password: S) -> Result<Vec<u8>, KeystoreError>
where
    P: AsRef<Path>,
    S: AsRef<[u8]>,
{
    // Read the file contents as string and deserialize it.
    let mut file = File::open(path)?;
    let mut contents = String::new();
    file.read_to_string(&mut contents)?;
    let keystore: EthKeystore = serde_json::from_str(&contents)?;

    // Derive the key.
    let key = match keystore.crypto.kdfparams {
        KdfparamsType::Pbkdf2 {
            c,
            dklen,
            prf: _,
            salt,
        } => {
            let mut key = vec![0u8; dklen as usize];
            pbkdf2::<Hmac<Sha256>>(password.as_ref(), &salt, c, key.as_mut_slice());
            key
        }
        KdfparamsType::Scrypt {
            dklen,
            n,
            p,
            r,
            salt,
        } => {
            let mut key = vec![0u8; dklen as usize];
            let log_n = (n as f32).log2() as u8;
            let scrypt_params = ScryptParams::new(log_n, r, p)?;
            scrypt(password.as_ref(), &salt, &scrypt_params, key.as_mut_slice())?;
            key
        }
    };

    // Derive the MAC from the derived key and ciphertext.
    let derived_mac = Keccak256::new()
        .chain(&key[16..32])
        .chain(&keystore.crypto.ciphertext)
        .finalize();

    if derived_mac.as_slice() != keystore.crypto.mac.as_slice() {
        return Err(KeystoreError::MacMismatch);
    }

    // Decrypt the private key bytes using AES-128-CTR
    dbg!(&key.len());
    dbg!(&keystore.crypto.cipherparams.iv.len());
    let mut decryptor = Ctr128::<Aes128>::new_var(&key[..16], &keystore.crypto.cipherparams.iv)?;

    let mut pk = keystore.crypto.ciphertext.clone();
    decryptor.apply_keystream(&mut pk);

    Ok(pk)
}

/// Encrypts the given private key using the [Scrypt](https://tools.ietf.org/html/rfc7914.html)
/// password-based key derivation function, and stores it in the provided directory.
///
/// # Example
///
/// ```no_run
/// use eth_keystore::encrypt_key;
/// use rand::RngCore;
/// use std::path::Path;
///
/// # async fn foobar() -> Result<(), Box<dyn std::error::Error>> {
/// let dir = Path::new("./keys");
/// let mut rng = rand::thread_rng();
///
/// // Construct a 32-byte random private key.
/// let mut private_key = vec![0u8; 32];
/// rng.fill_bytes(private_key.as_mut_slice());
///
/// let uuid = encrypt_key(&dir, &mut rng, &private_key, "password_to_keystore")?;
/// # Ok(())
/// # }
/// ```
pub fn encrypt_key<P, R, B, S>(
    dir: P,
    rng: &mut R,
    pk: B,
    password: S,
) -> Result<String, KeystoreError>
where
    P: AsRef<Path>,
    R: Rng + CryptoRng,
    B: AsRef<[u8]>,
    S: AsRef<[u8]>,
{
    // Generate a random salt.
    let mut salt = vec![0u8; DEFAULT_KEY_SIZE];
    rng.fill_bytes(salt.as_mut_slice());

    // Derive the key.
    let mut key = vec![0u8; DEFAULT_KDF_PARAMS_DKLEN as usize];
    let scrypt_params = ScryptParams::new(
        DEFAULT_KDF_PARAMS_LOG_N,
        DEFAULT_KDF_PARAMS_R,
        DEFAULT_KDF_PARAMS_P,
    )?;
    scrypt(password.as_ref(), &salt, &scrypt_params, key.as_mut_slice())?;

    // Encrypt the private key using AES-128-CTR.
    let mut iv = vec![0u8; DEFAULT_IV_SIZE];
    rng.fill_bytes(iv.as_mut_slice());

    let mut encryptor = Ctr128::<Aes128>::new_var(&key[..16], &iv)?;

    let mut ciphertext = pk.as_ref().to_vec();
    encryptor.apply_keystream(&mut ciphertext);

    // Calculate the MAC.
    let mac = Keccak256::new()
        .chain(&key[16..32])
        .chain(&ciphertext)
        .finalize();

    // Construct and serialize the encrypted JSON keystore.
    let id = Uuid::new_v4();
    let keystore = EthKeystore {
        id,
        version: 3,
        crypto: CryptoJson {
            cipher: String::from(DEFAULT_CIPHER),
            cipherparams: CipherparamsJson { iv },
            ciphertext: ciphertext.to_vec(),
            kdf: KdfType::Scrypt,
            kdfparams: KdfparamsType::Scrypt {
                dklen: DEFAULT_KDF_PARAMS_DKLEN,
                n: 2u32.pow(DEFAULT_KDF_PARAMS_LOG_N as u32),
                p: DEFAULT_KDF_PARAMS_P,
                r: DEFAULT_KDF_PARAMS_R,
                salt,
            },
            mac: mac.to_vec(),
        },
    };
    let contents = serde_json::to_string(&keystore)?;

    // Create a file in write-only mode, to store the encrypted JSON keystore.
    let mut file = File::create(dir.as_ref().join(id.to_string()))?;
    file.write_all(contents.as_bytes())?;

    Ok(id.to_string())
}
