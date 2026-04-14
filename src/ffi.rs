//! C ABI entrypoints for native iOS background push handling.
//!
//! These functions are callable from Swift via the static library without
//! requiring Flutter or FRB. The interface is intentionally minimal: pass in
//! directory paths and a timeout, get back a JSON string.

use std::ffi::{CStr, CString, c_char};
use std::time::Duration;

use crate::whitenoise::WhitenoiseConfig;
use crate::whitenoise::background_notifications::collect_notifications_after_push;

/// Collect notification updates after an iOS silent push wake.
///
/// Initializes Whitenoise if needed, refreshes relay subscriptions, and
/// collects any notification updates that arrive within the time window.
///
/// Returns a JSON string that the caller must free with [`wn_string_free`].
/// The JSON shape is:
///
/// ```json
/// {
///   "status": "new_data" | "no_data" | "failed",
///   "notifications": [ ... ],
///   "error": "..." // only present when status is "failed"
/// }
/// ```
///
/// # Safety
///
/// * `data_dir`, `logs_dir`, and `keyring_service_id` must be valid,
///   non-null, NUL-terminated UTF-8 C strings.
/// * The returned pointer must be freed with [`wn_string_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wn_collect_notifications_after_push(
    data_dir: *const c_char,
    logs_dir: *const c_char,
    keyring_service_id: *const c_char,
    max_wait_ms: u32,
) -> *mut c_char {
    // Null-pointer guards: CStr::from_ptr on a null pointer is UB and may not
    // be caught by catch_unwind. Check before entering the unsafe block.
    if data_dir.is_null() || logs_dir.is_null() || keyring_service_id.is_null() {
        let json = r#"{"status":"failed","notifications":[],"error":"null pointer passed to wn_collect_notifications_after_push"}"#;
        return CString::new(json)
            .expect("static JSON has no interior NUL")
            .into_raw();
    }

    let result = std::panic::catch_unwind(|| {
        // SAFETY: all three pointers confirmed non-null above; caller
        // guarantees they are valid NUL-terminated UTF-8 strings.
        let data_dir = unsafe { CStr::from_ptr(data_dir) }
            .to_str()
            .expect("data_dir is not valid UTF-8");
        let logs_dir = unsafe { CStr::from_ptr(logs_dir) }
            .to_str()
            .expect("logs_dir is not valid UTF-8");
        let keyring_service_id = unsafe { CStr::from_ptr(keyring_service_id) }
            .to_str()
            .expect("keyring_service_id is not valid UTF-8");

        let config = WhitenoiseConfig::new(
            std::path::Path::new(data_dir),
            std::path::Path::new(logs_dir),
            keyring_service_id,
        );
        let max_wait = Duration::from_millis(u64::from(max_wait_ms));

        let rt = tokio::runtime::Runtime::new().expect("failed to create Tokio runtime");
        let result = rt.block_on(collect_notifications_after_push(config, max_wait));

        serde_json::to_string(&result).expect("failed to serialize notification result")
    });

    let json = match result {
        Ok(json) => json,
        Err(_) => {
            r#"{"status":"failed","notifications":[],"error":"internal panic in wn_collect_notifications_after_push"}"#
                .to_string()
        }
    };

    CString::new(json)
        .expect("JSON contained interior NUL")
        .into_raw()
}

/// Free a string previously returned by [`wn_collect_notifications_after_push`].
///
/// # Safety
///
/// * `ptr` must have been returned by a `wn_*` function that allocates via `CString`.
/// * Must not be called more than once for the same pointer.
/// * Passing a null pointer is a safe no-op.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wn_string_free(ptr: *mut c_char) {
    if !ptr.is_null() {
        drop(unsafe { CString::from_raw(ptr) });
    }
}
