//! NIP-55 Android signer implementation using Amber.
//!
//! This module provides integration with Amber, a dedicated offline NIP-55/NIP-46
//! signer app for Android. It delegates all signing operations to Amber via
//! Android's ContentResolver, ensuring private keys never enter this process.
//!
//! # Usage
//!
//! Before using `AmberSigner`, you must initialize the Android JNI context by
//! calling `Whitenoise::init_android_context()` from your Android app's startup.
//!
//! # References
//!
//! - [NIP-55: Android Signer Application](https://github.com/nostr-protocol/nips/blob/master/55.md)
//! - [Amber Repository](https://github.com/greenart7c3/Amber)

#![cfg(target_os = "android")]

use jni::{
    JNIEnv, JavaVM,
    objects::{GlobalRef, JObject, JString, JValue},
};
use nostr_sdk::prelude::*;

use super::error::SignerError;

/// Simple error wrapper for SignerError::backend which requires std::error::Error.
#[derive(Debug)]
struct AmberSignerError(String);

impl std::fmt::Display for AmberSignerError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for AmberSignerError {}

/// Default Amber package name.
pub const AMBER_PACKAGE: &str = "com.greenart7c3.nostrsigner";

/// NIP-55 Content Resolver method names.
mod methods {
    pub const GET_PUBLIC_KEY: &str = "GET_PUBLIC_KEY";
    pub const SIGN_EVENT: &str = "SIGN_EVENT";
    pub const NIP44_ENCRYPT: &str = "NIP44_ENCRYPT";
    pub const NIP44_DECRYPT: &str = "NIP44_DECRYPT";
    pub const NIP04_ENCRYPT: &str = "NIP04_ENCRYPT";
    pub const NIP04_DECRYPT: &str = "NIP04_DECRYPT";
}

/// Result from a Content Resolver query.
#[derive(Debug, Default)]
struct QueryResult {
    /// The "result" column value.
    result: Option<String>,
    /// The "event" column value (for SIGN_EVENT).
    event: Option<String>,
    /// Whether the request was rejected.
    rejected: bool,
}

/// JNI context for Android ContentResolver access.
///
/// This struct holds the JVM reference and a global reference to the ContentResolver,
/// allowing calls from any thread.
pub struct AmberJniContext {
    vm: JavaVM,
    content_resolver: GlobalRef,
}

// Safety: JavaVM is thread-safe and GlobalRef is designed for cross-thread use
unsafe impl Send for AmberJniContext {}
unsafe impl Sync for AmberJniContext {}

impl std::fmt::Debug for AmberJniContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AmberJniContext")
            .field("vm", &"<JavaVM>")
            .field("content_resolver", &"<GlobalRef>")
            .finish()
    }
}

impl AmberJniContext {
    /// Create a new JNI context from the Android environment.
    ///
    /// # Safety
    ///
    /// This function must be called from a valid JNI environment with a valid
    /// ContentResolver object. The ContentResolver must remain valid for the
    /// lifetime of the application.
    ///
    /// # Arguments
    ///
    /// * `env` - The JNI environment
    /// * `content_resolver` - The Android ContentResolver object
    pub unsafe fn new(env: &mut JNIEnv, content_resolver: &JObject) -> Result<Self, SignerError> {
        let vm = env
            .get_java_vm()
            .map_err(|e| SignerError::JniError(format!("Failed to get JavaVM: {}", e)))?;

        let content_resolver = env
            .new_global_ref(content_resolver)
            .map_err(|e| SignerError::JniError(format!("Failed to create global ref: {}", e)))?;

        Ok(Self {
            vm,
            content_resolver,
        })
    }

    /// Query the Amber Content Provider.
    ///
    /// # Arguments
    ///
    /// * `method` - The NIP-55 method name (e.g., "SIGN_EVENT")
    /// * `params` - Key-value parameters for the request
    fn query(&self, method: &str, params: &[(&str, &str)]) -> Result<QueryResult, SignerError> {
        let mut env = self
            .vm
            .attach_current_thread()
            .map_err(|e| SignerError::JniError(format!("Failed to attach thread: {}", e)))?;

        // Build content URI: content://com.greenart7c3.nostrsigner.{METHOD}
        let uri_string = format!("content://{}.{}", AMBER_PACKAGE, method);

        // Parse the URI
        let uri = self.parse_uri(&mut env, &uri_string)?;

        // Build projection array from params
        let projection = self.build_projection(&mut env, params)?;

        // Call contentResolver.query(uri, projection, null, null, null)
        let cursor = env
            .call_method(
                &self.content_resolver,
                "query",
                "(Landroid/net/Uri;[Ljava/lang/String;Ljava/lang/String;[Ljava/lang/String;Ljava/lang/String;)Landroid/database/Cursor;",
                &[
                    JValue::Object(&uri),
                    JValue::Object(&projection),
                    JValue::Object(&JObject::null()),
                    JValue::Object(&JObject::null()),
                    JValue::Object(&JObject::null()),
                ],
            )
            .map_err(|e| SignerError::JniError(format!("Failed to call query: {}", e)))?
            .l()
            .map_err(|e| SignerError::JniError(format!("Failed to get cursor object: {}", e)))?;

        // Null cursor = Amber not installed or pubkey not found
        if cursor.is_null() {
            return Err(SignerError::AmberNotInstalled);
        }

        // Parse cursor columns
        let result = self.parse_cursor(&mut env, &cursor)?;

        // Close the cursor
        let _ = env.call_method(&cursor, "close", "()V", &[]);

        Ok(result)
    }

    /// Parse a URI string into an Android Uri object.
    fn parse_uri<'a>(
        &self,
        env: &mut JNIEnv<'a>,
        uri_string: &str,
    ) -> Result<JObject<'a>, SignerError> {
        let uri_class = env
            .find_class("android/net/Uri")
            .map_err(|e| SignerError::JniError(format!("Failed to find Uri class: {}", e)))?;

        let uri_str = env
            .new_string(uri_string)
            .map_err(|e| SignerError::JniError(format!("Failed to create string: {}", e)))?;

        env.call_static_method(
            uri_class,
            "parse",
            "(Ljava/lang/String;)Landroid/net/Uri;",
            &[JValue::Object(&uri_str)],
        )
        .map_err(|e| SignerError::JniError(format!("Failed to call Uri.parse: {}", e)))?
        .l()
        .map_err(|e| SignerError::JniError(format!("Failed to get Uri object: {}", e)))
    }

    /// Build a String[] projection array from parameters.
    fn build_projection<'a>(
        &self,
        env: &mut JNIEnv<'a>,
        params: &[(&str, &str)],
    ) -> Result<JObject<'a>, SignerError> {
        let string_class = env
            .find_class("java/lang/String")
            .map_err(|e| SignerError::JniError(format!("Failed to find String class: {}", e)))?;

        // Create array of size params.len() * 2 (key-value pairs flattened)
        // NIP-55 expects projection as: [value1, value2, ...]
        // The keys are implicit based on the method
        let array_size = params.len() as i32;
        let array = env
            .new_object_array(array_size, string_class, JObject::null())
            .map_err(|e| SignerError::JniError(format!("Failed to create array: {}", e)))?;

        for (i, (_, value)) in params.iter().enumerate() {
            let jvalue = env.new_string(value).map_err(|e| {
                SignerError::JniError(format!("Failed to create param string: {}", e))
            })?;
            env.set_object_array_element(&array, i as i32, jvalue)
                .map_err(|e| {
                    SignerError::JniError(format!("Failed to set array element: {}", e))
                })?;
        }

        Ok(array.into())
    }

    /// Parse cursor columns into a QueryResult.
    fn parse_cursor(&self, env: &mut JNIEnv, cursor: &JObject) -> Result<QueryResult, SignerError> {
        let mut result = QueryResult::default();

        // Move cursor to first row
        let has_data = env
            .call_method(cursor, "moveToFirst", "()Z", &[])
            .map_err(|e| SignerError::JniError(format!("Failed to moveToFirst: {}", e)))?
            .z()
            .map_err(|e| SignerError::JniError(format!("Failed to get boolean: {}", e)))?;

        if !has_data {
            return Ok(result);
        }

        // Try to get each column
        result.result = self.get_column_string(env, cursor, "result")?;
        result.event = self.get_column_string(env, cursor, "event")?;

        // Check rejected column
        if let Some(rejected_str) = self.get_column_string(env, cursor, "rejected")? {
            result.rejected = rejected_str == "true";
        }

        Ok(result)
    }

    /// Get a string column value from a cursor.
    fn get_column_string(
        &self,
        env: &mut JNIEnv,
        cursor: &JObject,
        column_name: &str,
    ) -> Result<Option<String>, SignerError> {
        let col_name_str = env
            .new_string(column_name)
            .map_err(|e| SignerError::JniError(format!("Failed to create column name: {}", e)))?;

        // Get column index
        let column_index = env
            .call_method(
                cursor,
                "getColumnIndex",
                "(Ljava/lang/String;)I",
                &[JValue::Object(&col_name_str)],
            )
            .map_err(|e| SignerError::JniError(format!("Failed to getColumnIndex: {}", e)))?
            .i()
            .map_err(|e| SignerError::JniError(format!("Failed to get int: {}", e)))?;

        if column_index < 0 {
            return Ok(None);
        }

        // Get string value
        let jstring = env
            .call_method(
                cursor,
                "getString",
                "(I)Ljava/lang/String;",
                &[JValue::Int(column_index)],
            )
            .map_err(|e| SignerError::JniError(format!("Failed to getString: {}", e)))?
            .l()
            .map_err(|e| SignerError::JniError(format!("Failed to get string object: {}", e)))?;

        if jstring.is_null() {
            return Ok(None);
        }

        let jstring: JString = jstring.into();
        let value = env
            .get_string(&jstring)
            .map_err(|e| SignerError::JniError(format!("Failed to convert string: {}", e)))?;

        Ok(Some(value.into()))
    }
}

/// NIP-55 Android signer using Amber.
///
/// This signer delegates all cryptographic operations to the Amber app via
/// Android's ContentResolver. The private key never enters this process.
#[derive(Debug)]
pub struct AmberSigner {
    /// The public key this signer operates for.
    pubkey: PublicKey,
    /// Package name of the signer app.
    package_name: String,
}

impl AmberSigner {
    /// Create a new AmberSigner for the given public key.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - The public key to sign for
    pub fn new(pubkey: PublicKey) -> Self {
        Self {
            pubkey,
            package_name: AMBER_PACKAGE.to_string(),
        }
    }

    /// Create a new AmberSigner with a custom package name.
    ///
    /// # Arguments
    ///
    /// * `pubkey` - The public key to sign for
    /// * `package_name` - Custom signer app package name
    pub fn with_package(pubkey: PublicKey, package_name: String) -> Self {
        Self {
            pubkey,
            package_name,
        }
    }

    /// Get a reference to the JNI context.
    fn get_context() -> Result<&'static AmberJniContext, SignerError> {
        super::android_context::get().ok_or(SignerError::AndroidContextNotInitialized)
    }

    /// Execute a query on the Amber Content Provider.
    async fn query(
        &self,
        method: &str,
        params: &[(&str, &str)],
    ) -> Result<QueryResult, SignerError> {
        let ctx = Self::get_context()?;
        let method = method.to_string();
        let params: Vec<(String, String)> = params
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        // Move JNI call to blocking thread pool to avoid blocking the async runtime
        tokio::task::spawn_blocking(move || {
            let params_refs: Vec<(&str, &str)> = params
                .iter()
                .map(|(k, v)| (k.as_str(), v.as_str()))
                .collect();
            ctx.query(&method, &params_refs)
        })
        .await
        .map_err(|e| SignerError::JniError(format!("Task join error: {}", e)))?
    }
}

impl NostrSigner for AmberSigner {
    fn backend(&self) -> SignerBackend<'_> {
        SignerBackend::Custom("amber-nip55".into())
    }

    fn get_public_key(&self) -> BoxedFuture<'_, Result<PublicKey, nostr_sdk::signer::SignerError>> {
        Box::pin(async move { Ok(self.pubkey) })
    }

    fn sign_event(
        &self,
        unsigned: UnsignedEvent,
    ) -> BoxedFuture<'_, Result<Event, nostr_sdk::signer::SignerError>> {
        Box::pin(async move {
            let event_json = serde_json::to_string(&unsigned)
                .map_err(|e| nostr_sdk::signer::SignerError::backend(AmberSignerError(e.to_string())))?;

            let result = self
                .query(
                    methods::SIGN_EVENT,
                    &[
                        ("event", &event_json),
                        ("current_user", &self.pubkey.to_hex()),
                    ],
                )
                .await
                .map_err(|e| nostr_sdk::signer::SignerError::backend(AmberSignerError(e.to_string())))?;

            if result.rejected {
                return Err(nostr_sdk::signer::SignerError::backend(
                    AmberSignerError("User rejected signing request".to_string()),
                ));
            }

            // NIP-55 returns signed event in "event" column
            let event_str = result.event.ok_or_else(|| {
                nostr_sdk::signer::SignerError::backend(AmberSignerError("No signed event returned from Amber".to_string()))
            })?;

            serde_json::from_str(&event_str)
                .map_err(|e| nostr_sdk::signer::SignerError::backend(AmberSignerError(e.to_string())))
        })
    }

    fn nip44_encrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        content: &'a str,
    ) -> BoxedFuture<'a, Result<String, nostr_sdk::signer::SignerError>> {
        Box::pin(async move {
            let result = self
                .query(
                    methods::NIP44_ENCRYPT,
                    &[
                        ("plaintext", content),
                        ("pubkey", &public_key.to_hex()),
                        ("current_user", &self.pubkey.to_hex()),
                    ],
                )
                .await
                .map_err(|e| nostr_sdk::signer::SignerError::backend(AmberSignerError(e.to_string())))?;

            if result.rejected {
                return Err(nostr_sdk::signer::SignerError::backend(
                    AmberSignerError("User rejected encryption request".to_string()),
                ));
            }

            result.result.ok_or_else(|| {
                nostr_sdk::signer::SignerError::backend(AmberSignerError("No encrypted content returned from Amber".to_string()))
            })
        })
    }

    fn nip44_decrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        content: &'a str,
    ) -> BoxedFuture<'a, Result<String, nostr_sdk::signer::SignerError>> {
        Box::pin(async move {
            let result = self
                .query(
                    methods::NIP44_DECRYPT,
                    &[
                        ("ciphertext", content),
                        ("pubkey", &public_key.to_hex()),
                        ("current_user", &self.pubkey.to_hex()),
                    ],
                )
                .await
                .map_err(|e| nostr_sdk::signer::SignerError::backend(AmberSignerError(e.to_string())))?;

            if result.rejected {
                return Err(nostr_sdk::signer::SignerError::backend(
                    AmberSignerError("User rejected decryption request".to_string()),
                ));
            }

            result.result.ok_or_else(|| {
                nostr_sdk::signer::SignerError::backend(AmberSignerError("No decrypted content returned from Amber".to_string()))
            })
        })
    }

    fn nip04_encrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        content: &'a str,
    ) -> BoxedFuture<'a, Result<String, nostr_sdk::signer::SignerError>> {
        Box::pin(async move {
            let result = self
                .query(
                    methods::NIP04_ENCRYPT,
                    &[
                        ("plaintext", content),
                        ("pubkey", &public_key.to_hex()),
                        ("current_user", &self.pubkey.to_hex()),
                    ],
                )
                .await
                .map_err(|e| nostr_sdk::signer::SignerError::backend(AmberSignerError(e.to_string())))?;

            if result.rejected {
                return Err(nostr_sdk::signer::SignerError::backend(
                    AmberSignerError("User rejected encryption request".to_string()),
                ));
            }

            result.result.ok_or_else(|| {
                nostr_sdk::signer::SignerError::backend(AmberSignerError("No encrypted content returned from Amber".to_string()))
            })
        })
    }

    fn nip04_decrypt<'a>(
        &'a self,
        public_key: &'a PublicKey,
        content: &'a str,
    ) -> BoxedFuture<'a, Result<String, nostr_sdk::signer::SignerError>> {
        Box::pin(async move {
            let result = self
                .query(
                    methods::NIP04_DECRYPT,
                    &[
                        ("ciphertext", content),
                        ("pubkey", &public_key.to_hex()),
                        ("current_user", &self.pubkey.to_hex()),
                    ],
                )
                .await
                .map_err(|e| nostr_sdk::signer::SignerError::backend(AmberSignerError(e.to_string())))?;

            if result.rejected {
                return Err(nostr_sdk::signer::SignerError::backend(
                    AmberSignerError("User rejected decryption request".to_string()),
                ));
            }

            result.result.ok_or_else(|| {
                nostr_sdk::signer::SignerError::backend(AmberSignerError("No decrypted content returned from Amber".to_string()))
            })
        })
    }
}
