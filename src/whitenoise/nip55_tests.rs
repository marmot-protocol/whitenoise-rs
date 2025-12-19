use crate::whitenoise::{nip55_signer::Nip55FlutterCallback, test_utils::create_mock_whitenoise};
use nostr_sdk::prelude::*;
use serde_json::json;
use std::sync::Arc;

// Mock Flutter Callback
#[derive(Debug)]
struct MockFlutterCallback {
    pubkey: PublicKey,
}

impl MockFlutterCallback {
    #[allow(dead_code)]
    fn new(pubkey: PublicKey) -> Self {
        Self { pubkey }
    }
}

impl Nip55FlutterCallback for MockFlutterCallback {
    fn call_nip55_method(&self, method: &str, params: &str) -> Result<String, String> {
        match method {
            "get_public_key" => Ok(self.pubkey.to_hex()),
            "sign_event" => {
                let _params_val: serde_json::Value =
                    serde_json::from_str(params).map_err(|e| e.to_string())?;
                Err("Signing not implemented in simple mock".to_string())
            }
            "nip04_encrypt" => Ok("encrypted_content".to_string()),
            "nip04_decrypt" => Ok("decrypted_content".to_string()),
            "nip44_encrypt" => Ok("encrypted_content".to_string()),
            "nip44_decrypt" => Ok("decrypted_content".to_string()),
            _ => Err(format!("Unknown method: {}", method)),
        }
    }
}

// A more advanced mock that can actually sign
#[derive(Debug)]
struct SigningMockFlutterCallback {
    keys: Keys,
}

impl SigningMockFlutterCallback {
    fn new(keys: Keys) -> Self {
        Self { keys }
    }
}

impl Nip55FlutterCallback for SigningMockFlutterCallback {
    fn call_nip55_method(&self, method: &str, params: &str) -> Result<String, String> {
        match method {
            "get_public_key" => {
                // Return just the result string, not a JSON object
                Ok(json!({
                    "result": self.keys.public_key().to_hex(),
                    "rejected": false,
                    "error": null
                })
                .to_string())
            }
            "sign_event" => {
                let params_vec: Vec<serde_json::Value> =
                    serde_json::from_str(params).map_err(|e| e.to_string())?;
                // The first param is the event json string or object
                if let Some(event_val) = params_vec.first() {
                    let unsigned_event = if event_val.is_string() {
                        let event_str = event_val.as_str().unwrap();
                        UnsignedEvent::from_json(event_str).map_err(|e| e.to_string())?
                    } else {
                        serde_json::from_value(event_val.clone()).map_err(|e| e.to_string())?
                    };

                    // Sign it
                    let keys_clone = self.keys.clone();
                    let unsigned_event_clone = unsigned_event.clone();

                    let event = std::thread::spawn(move || {
                        tokio::runtime::Builder::new_current_thread()
                            .enable_all()
                            .build()
                            .unwrap()
                            .block_on(async { unsigned_event_clone.sign(&keys_clone).await })
                    })
                    .join()
                    .expect("Thread panicked")
                    .map_err(|e| e.to_string())?;

                    Ok(json!({
                       "result": event.as_json(),
                       "rejected": false,
                       "error": null
                    })
                    .to_string())
                } else {
                    Err("Missing event param".to_string())
                }
            }
            "nip44_decrypt" => {
                let params_vec: Vec<serde_json::Value> =
                    serde_json::from_str(params).map_err(|e| e.to_string())?;
                if let (Some(pubkey_val), Some(ciphertext_val)) =
                    (params_vec.first(), params_vec.get(1))
                {
                    let pubkey = PublicKey::parse(pubkey_val.as_str().unwrap())
                        .map_err(|e| e.to_string())?;
                    let ciphertext = ciphertext_val.as_str().unwrap();

                    let secret_key = self.keys.secret_key();
                    let plaintext =
                        nostr_sdk::nips::nip44::decrypt(secret_key, &pubkey, ciphertext)
                            .map_err(|e| e.to_string())?;
                    Ok(json!({
                       "result": plaintext,
                       "rejected": false,
                       "error": null
                    })
                    .to_string())
                } else {
                    Err("Missing params for nip44_decrypt".to_string())
                }
            }
            "nip44_encrypt" => {
                let params_vec: Vec<serde_json::Value> =
                    serde_json::from_str(params).map_err(|e| e.to_string())?;
                if let (Some(pubkey_val), Some(plaintext_val)) =
                    (params_vec.first(), params_vec.get(1))
                {
                    let pubkey = PublicKey::parse(pubkey_val.as_str().unwrap())
                        .map_err(|e| e.to_string())?;
                    let plaintext = plaintext_val.as_str().unwrap();

                    let secret_key = self.keys.secret_key();
                    let content = nostr_sdk::nips::nip44::encrypt(
                        secret_key,
                        &pubkey,
                        plaintext,
                        nostr_sdk::nips::nip44::Version::V2,
                    )
                    .map_err(|e| e.to_string())?;
                    Ok(json!({
                       "result": content,
                       "rejected": false,
                       "error": null
                    })
                    .to_string())
                } else {
                    Err("Missing params for nip44_encrypt".to_string())
                }
            }
            _ => Ok("{}".to_string()),
        }
    }
}

#[tokio::test]
async fn test_login_with_nip55_flow() {
    let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

    // 1. Setup Mock Callback
    let test_keys = Keys::generate();
    let callback = Arc::new(SigningMockFlutterCallback::new(test_keys.clone()));

    // 2. Set the callback in whitenoise
    whitenoise.set_nip55_flutter_callback(callback).await;

    // 3. Perform Login
    let account = whitenoise
        .login_with_nip55()
        .await
        .expect("Login with NIP-55 failed");

    // 4. Verify Account
    assert_eq!(account.pubkey, test_keys.public_key());
    assert!(whitenoise.is_nip55_enabled(&account));

    // 5. Verify Signer
    let signer = whitenoise
        .get_signer_for_account(&account)
        .await
        .expect("Failed to get signer");

    // Try signing an event
    let event_builder = EventBuilder::text_note("Hello NIP-55");
    let unsigned_event = event_builder.build(test_keys.public_key());
    let event = signer
        .sign_event(unsigned_event)
        .await
        .expect("Failed to sign event");

    assert_eq!(event.pubkey, test_keys.public_key());
    assert!(event.verify().is_ok());
}

#[tokio::test]
async fn test_get_signer_for_nip55_vs_secrets_store() {
    let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;

    // Case 1: Normal Account (Secrets Store)
    // create_test_account DOES NOT store keys in secrets store by default, we have to do it manually.
    let (normal_account, keys) =
        crate::whitenoise::test_utils::create_test_account(&whitenoise).await;

    // Attempt to store, but don't panic if it fails due to environment
    if let Err(e) = whitenoise.secrets_store.store_private_key(&keys) {
        println!(
            "Skipping SecretsStore test part due to store failure: {}",
            e
        );
        return;
    }

    let signer_normal = whitenoise
        .get_signer_for_account(&normal_account)
        .await
        .unwrap();

    use crate::whitenoise::nip55_signer::WhitenoiseSigner;
    match signer_normal {
        WhitenoiseSigner::Keys(_) => (),
        _ => panic!("Expected Keys signer for normal account"),
    }

    // Case 2: NIP-55 Account
    let nip55_keys = Keys::generate();
    let callback = Arc::new(SigningMockFlutterCallback::new(nip55_keys.clone()));
    whitenoise.set_nip55_flutter_callback(callback).await;
    let nip55_account = whitenoise.login_with_nip55().await.unwrap();

    let signer_nip55 = whitenoise
        .get_signer_for_account(&nip55_account)
        .await
        .unwrap();
    match signer_nip55 {
        WhitenoiseSigner::Nip55(_) => (),
        _ => panic!("Expected Nip55 signer for NIP-55 account"),
    }
}

#[tokio::test]
async fn test_disable_nip55_signer() {
    let (whitenoise, _data_temp, _logs_temp) = create_mock_whitenoise().await;
    let keys = Keys::generate();
    let callback = Arc::new(SigningMockFlutterCallback::new(keys.clone()));
    whitenoise.set_nip55_flutter_callback(callback).await;

    let account = whitenoise.login_with_nip55().await.unwrap();
    assert!(whitenoise.is_nip55_enabled(&account));

    whitenoise.disable_nip55_signer(&account).await;
    assert!(!whitenoise.is_nip55_enabled(&account));

    // Getting signer now should fail because keys are not in secrets store for this account
    // (login_with_nip55 does not store private keys)
    let result = whitenoise.get_signer_for_account(&account).await;
    assert!(result.is_err());
}
