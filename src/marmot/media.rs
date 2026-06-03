//! Darkmatter-compatible encrypted chat media primitives.

use chacha20poly1305::aead::{Aead, Payload};
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};

use crate::whitenoise::error::{Result, WhitenoiseError};

pub(crate) const ENCRYPTED_MEDIA_VERSION: &str = "mip04-v2";
pub(crate) const ENCRYPTED_MEDIA_EXPORTER_LABEL: &str = "marmot/encrypted-media";

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct EncryptedChatMedia {
    pub(crate) encrypted_data: Vec<u8>,
    pub(crate) original_hash: [u8; 32],
    pub(crate) encrypted_hash: [u8; 32],
    pub(crate) mime_type: String,
    pub(crate) filename: String,
    pub(crate) nonce: [u8; 12],
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct ChatMediaReference<'a> {
    pub(crate) original_hash: &'a [u8; 32],
    pub(crate) mime_type: &'a str,
    pub(crate) filename: &'a str,
    pub(crate) scheme_version: &'a str,
    pub(crate) nonce: [u8; 12],
}

pub(crate) fn canonical_media_type(value: &str) -> Result<String> {
    let media_type = value
        .split(';')
        .next()
        .unwrap_or_default()
        .trim_matches(|c: char| c.is_ascii_whitespace())
        .to_ascii_lowercase();
    if media_type.is_empty() || !media_type.contains('/') || media_type.len() > 100 {
        return Err(WhitenoiseError::InvalidInput(
            "media type must be a MIME type".to_string(),
        ));
    }

    Ok(match media_type.as_str() {
        "image/jpg" => "image/jpeg".to_string(),
        other => other.to_string(),
    })
}

pub(crate) fn encrypt_chat_media(
    exporter_secret: &[u8],
    plaintext: &[u8],
    mime_type: &str,
    filename: &str,
) -> Result<EncryptedChatMedia> {
    let mut nonce = [0_u8; 12];
    getrandom::fill(&mut nonce).map_err(|error| {
        WhitenoiseError::Internal(format!("failed to generate chat media nonce: {error}"))
    })?;

    encrypt_chat_media_with_nonce(exporter_secret, plaintext, mime_type, filename, nonce)
}

pub(crate) fn decrypt_chat_media(
    exporter_secret: &[u8],
    encrypted_data: &[u8],
    reference: ChatMediaReference<'_>,
) -> Result<Vec<u8>> {
    if reference.scheme_version != ENCRYPTED_MEDIA_VERSION {
        return Err(WhitenoiseError::InvalidInput(format!(
            "unsupported chat media scheme version: {}",
            reference.scheme_version
        )));
    }

    let mime_type = canonical_media_type(reference.mime_type)?;
    let filename = canonical_filename(reference.filename)?;
    let key = derive_media_file_key(
        exporter_secret,
        reference.original_hash,
        &mime_type,
        &filename,
    )?;
    let aad = media_aad(reference.original_hash, &mime_type, &filename);
    let cipher = ChaCha20Poly1305::new_from_slice(&key)
        .map_err(|_| WhitenoiseError::MediaCache("invalid media key length".to_string()))?;
    let plaintext = cipher
        .decrypt(
            Nonce::from_slice(&reference.nonce),
            Payload {
                msg: encrypted_data,
                aad: &aad,
            },
        )
        .map_err(|_| WhitenoiseError::MediaCache("chat media decryption failed".to_string()))?;

    let actual_hash: [u8; 32] = Sha256::digest(&plaintext).into();
    if actual_hash != *reference.original_hash {
        return Err(WhitenoiseError::HashMismatch {
            expected: hex::encode(reference.original_hash),
            actual: hex::encode(actual_hash),
        });
    }

    Ok(plaintext)
}

fn encrypt_chat_media_with_nonce(
    exporter_secret: &[u8],
    plaintext: &[u8],
    mime_type: &str,
    filename: &str,
    nonce: [u8; 12],
) -> Result<EncryptedChatMedia> {
    if plaintext.is_empty() {
        return Err(WhitenoiseError::InvalidInput(
            "chat media plaintext cannot be empty".to_string(),
        ));
    }

    let mime_type = canonical_media_type(mime_type)?;
    let filename = canonical_filename(filename)?;
    let original_hash: [u8; 32] = Sha256::digest(plaintext).into();
    let key = derive_media_file_key(exporter_secret, &original_hash, &mime_type, &filename)?;
    let aad = media_aad(&original_hash, &mime_type, &filename);
    let cipher = ChaCha20Poly1305::new_from_slice(&key)
        .map_err(|_| WhitenoiseError::MediaCache("invalid media key length".to_string()))?;
    let encrypted_data = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad: &aad,
            },
        )
        .map_err(|_| WhitenoiseError::MediaCache("chat media encryption failed".to_string()))?;

    Ok(EncryptedChatMedia {
        encrypted_hash: Sha256::digest(&encrypted_data).into(),
        encrypted_data,
        original_hash,
        mime_type,
        filename,
        nonce,
    })
}

fn canonical_filename(value: &str) -> Result<String> {
    let filename = value.trim_matches(|c: char| c.is_ascii_whitespace());
    if filename.is_empty() {
        return Err(WhitenoiseError::InvalidInput(
            "chat media filename cannot be empty".to_string(),
        ));
    }

    Ok(filename.to_string())
}

fn derive_media_file_key(
    exporter_secret: &[u8],
    file_hash: &[u8; 32],
    mime_type: &str,
    filename: &str,
) -> Result<[u8; 32]> {
    let hkdf = Hkdf::<Sha256>::from_prk(exporter_secret).map_err(|_| {
        WhitenoiseError::MediaCache("invalid encrypted-media exporter secret".to_string())
    })?;
    let mut key = [0_u8; 32];
    hkdf.expand(&media_key_info(file_hash, mime_type, filename), &mut key)
        .map_err(|_| WhitenoiseError::MediaCache("media key derivation failed".to_string()))?;
    Ok(key)
}

fn media_key_info(file_hash: &[u8; 32], mime_type: &str, filename: &str) -> Vec<u8> {
    let mut info = Vec::with_capacity(
        ENCRYPTED_MEDIA_VERSION.len() + 1 + 32 + 1 + mime_type.len() + 1 + filename.len() + 4,
    );
    info.extend_from_slice(ENCRYPTED_MEDIA_VERSION.as_bytes());
    info.push(0);
    info.extend_from_slice(file_hash);
    info.push(0);
    info.extend_from_slice(mime_type.as_bytes());
    info.push(0);
    info.extend_from_slice(filename.as_bytes());
    info.push(0);
    info.extend_from_slice(b"key");
    info
}

fn media_aad(file_hash: &[u8; 32], mime_type: &str, filename: &str) -> Vec<u8> {
    let mut aad = Vec::with_capacity(
        ENCRYPTED_MEDIA_VERSION.len() + 1 + 32 + 1 + mime_type.len() + 1 + filename.len(),
    );
    aad.extend_from_slice(ENCRYPTED_MEDIA_VERSION.as_bytes());
    aad.push(0);
    aad.extend_from_slice(file_hash);
    aad.push(0);
    aad.extend_from_slice(mime_type.as_bytes());
    aad.push(0);
    aad.extend_from_slice(filename.as_bytes());
    aad
}

#[cfg(test)]
mod tests {
    use super::{
        ChatMediaReference, ENCRYPTED_MEDIA_VERSION, canonical_media_type, decrypt_chat_media,
        encrypt_chat_media_with_nonce,
    };

    #[test]
    fn encrypt_then_decrypt_uses_darkmatter_mip04_schedule() {
        let exporter_secret = [7_u8; 32];
        let plaintext = b"darkmatter encrypted media";
        let nonce = [9_u8; 12];

        let encrypted = encrypt_chat_media_with_nonce(
            &exporter_secret,
            plaintext,
            "IMAGE/JPG; charset=binary",
            " photo.jpg ",
            nonce,
        )
        .unwrap();

        assert_eq!(encrypted.mime_type, "image/jpeg");
        assert_eq!(encrypted.filename, "photo.jpg");
        assert_ne!(encrypted.encrypted_data, plaintext);

        let decrypted = decrypt_chat_media(
            &exporter_secret,
            &encrypted.encrypted_data,
            ChatMediaReference {
                original_hash: &encrypted.original_hash,
                mime_type: &encrypted.mime_type,
                filename: &encrypted.filename,
                scheme_version: ENCRYPTED_MEDIA_VERSION,
                nonce,
            },
        )
        .unwrap();

        assert_eq!(decrypted, plaintext);
    }

    #[test]
    fn decrypt_rejects_wrong_filename_aad() {
        let exporter_secret = [7_u8; 32];
        let plaintext = b"darkmatter encrypted media";
        let nonce = [9_u8; 12];
        let encrypted = encrypt_chat_media_with_nonce(
            &exporter_secret,
            plaintext,
            "image/png",
            "photo.png",
            nonce,
        )
        .unwrap();

        let error = decrypt_chat_media(
            &exporter_secret,
            &encrypted.encrypted_data,
            ChatMediaReference {
                original_hash: &encrypted.original_hash,
                mime_type: &encrypted.mime_type,
                filename: "other.png",
                scheme_version: ENCRYPTED_MEDIA_VERSION,
                nonce,
            },
        )
        .unwrap_err();

        assert!(error.to_string().contains("decryption failed"));
    }

    #[test]
    fn canonical_media_type_matches_darkmatter_rules() {
        assert_eq!(
            canonical_media_type(" Image/JPG ; charset=utf-8 ").unwrap(),
            "image/jpeg"
        );
        assert!(canonical_media_type("not-a-mime").is_err());
    }
}
