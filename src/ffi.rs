//! C ABI entrypoints for native iOS background push handling.
//!
//! These functions are callable from Swift via the static library without
//! requiring Flutter or FRB. The interface is intentionally minimal: pass in
//! directory paths and a timeout, get back a JSON string.
//!
//! # Contract guarantees
//!
//! * [`wn_collect_notifications_after_push`] **never returns NULL** under any
//!   circumstances — callers can unconditionally treat the result as a valid,
//!   NUL-terminated UTF-8 C string.
//! * It **never panics across the FFI boundary** — all internal panics are
//!   caught and converted to a failure JSON response.
//! * It **always returns valid JSON** conforming to the documented shape.

use std::ffi::{CStr, CString, c_char};
use std::time::Duration;

use crate::whitenoise::WhitenoiseConfig;
use crate::whitenoise::background_notifications::{
    BackgroundNotificationResult, collect_notifications_after_push,
};

/// Static fallback JSON used when dynamic string construction fails.
/// This string is guaranteed to:
/// * be valid JSON conforming to the documented shape,
/// * contain no interior NUL bytes,
/// * be convertible to a `CString` without allocation failure.
const FALLBACK_FAILURE_JSON: &str =
    r#"{"status":"failed","notifications":[],"error":"internal error: failed to build response"}"#;

/// Collect notification updates after an iOS silent push wake.
///
/// Initializes Whitenoise if needed, refreshes relay subscriptions, and
/// collects any notification updates that arrive within the time window.
///
/// Returns a JSON string that the caller must free with [`wn_string_free`].
/// **This function never returns NULL** — every code path produces a valid
/// JSON response. The JSON shape is:
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
/// * `data_dir`, `logs_dir`, and `keyring_service_id` should be valid
///   NUL-terminated UTF-8 C strings. Null pointers and invalid UTF-8 are
///   handled gracefully by returning a failure JSON response.
/// * The returned pointer must be freed with [`wn_string_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wn_collect_notifications_after_push(
    data_dir: *const c_char,
    logs_dir: *const c_char,
    keyring_service_id: *const c_char,
    max_wait_ms: u32,
) -> *mut c_char {
    // Run the entire body under catch_unwind so no panic can ever cross the
    // FFI boundary. The closure produces a `String` which we then turn into
    // a CString with fallbacks.
    let json = std::panic::catch_unwind(|| {
        // SAFETY: these reads are inside catch_unwind. Null pointers are
        // checked explicitly before CStr::from_ptr; invalid UTF-8 is handled
        // via to_str() without panicking.
        let data_dir = match unsafe { c_str_to_string(data_dir, "data_dir") } {
            Ok(s) => s,
            Err(json) => return json,
        };
        let logs_dir = match unsafe { c_str_to_string(logs_dir, "logs_dir") } {
            Ok(s) => s,
            Err(json) => return json,
        };
        let keyring_service_id =
            match unsafe { c_str_to_string(keyring_service_id, "keyring_service_id") } {
                Ok(s) => s,
                Err(json) => return json,
            };

        let config = WhitenoiseConfig::new(
            std::path::Path::new(&data_dir),
            std::path::Path::new(&logs_dir),
            &keyring_service_id,
        );
        let max_wait = Duration::from_millis(u64::from(max_wait_ms));

        // Use a current-thread runtime: this FFI call is a bounded, single
        // async operation driven to completion via block_on. A multi-threaded
        // runtime would spawn worker threads we don't need for a background
        // iOS push handler. `enable_all()` keeps net + time + the blocking
        // worker pool available for SQLite and any spawn_blocking callers
        // down the call stack.
        let rt = match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt,
            Err(e) => {
                return result_to_json(BackgroundNotificationResult::failed(format!(
                    "failed to create tokio runtime: {e}"
                )));
            }
        };

        let result = rt.block_on(collect_notifications_after_push(config, max_wait));
        result_to_json(result)
    })
    .unwrap_or_else(|_| {
        // Panic escaped. Log is not useful here (tracing may be down). Fall
        // back to a static failure JSON that cannot fail further operations.
        r#"{"status":"failed","notifications":[],"error":"internal panic in wn_collect_notifications_after_push"}"#
            .to_string()
    });

    // Convert to CString. If there's an interior NUL (shouldn't happen with
    // our shapes, but defensively), sanitize by stripping NUL bytes. If even
    // that fails, fall back to the static JSON.
    string_to_raw(json)
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

/// Convert a C string pointer into an owned `String`, returning a failure
/// JSON payload on null or invalid UTF-8.
///
/// # Safety
///
/// Caller must ensure `ptr` is either null or a valid NUL-terminated C string.
unsafe fn c_str_to_string(ptr: *const c_char, field: &str) -> Result<String, String> {
    if ptr.is_null() {
        return Err(result_to_json(BackgroundNotificationResult::failed(
            format!("null pointer for `{field}`"),
        )));
    }
    // SAFETY: ptr is non-null; caller guarantees NUL termination.
    let cstr = unsafe { CStr::from_ptr(ptr) };
    match cstr.to_str() {
        Ok(s) => Ok(s.to_string()),
        Err(e) => Err(result_to_json(BackgroundNotificationResult::failed(
            format!("`{field}` is not valid UTF-8: {e}"),
        ))),
    }
}

/// Serialize a `BackgroundNotificationResult` to JSON, falling back to a
/// hardcoded failure payload if serialization itself fails.
fn result_to_json(result: BackgroundNotificationResult) -> String {
    serde_json::to_string(&result).unwrap_or_else(|e| {
        tracing::error!(
            target: "whitenoise::ffi",
            "Failed to serialize BackgroundNotificationResult: {}",
            e
        );
        FALLBACK_FAILURE_JSON.to_string()
    })
}

/// Convert an owned `String` of JSON into a leaked C string pointer.
///
/// Guaranteed to never return NULL: if the input contains interior NUL bytes
/// or CString construction fails for any reason, the static fallback JSON is
/// used instead.
fn string_to_raw(json: String) -> *mut c_char {
    // CString::new rejects interior NULs. First try the happy path.
    if let Ok(cstr) = CString::new(json.clone()) {
        return cstr.into_raw();
    }

    // Interior NUL present. Strip NUL bytes and retry.
    let sanitized: String = json.chars().filter(|c| *c != '\0').collect();
    if let Ok(cstr) = CString::new(sanitized) {
        return cstr.into_raw();
    }

    // Last-resort static fallback. This is statically known to not contain
    // NUL bytes, so CString::new cannot fail — but we still guard with
    // unwrap_or_else to absolutely guarantee no NULL return.
    CString::new(FALLBACK_FAILURE_JSON)
        .unwrap_or_else(|_| {
            // This branch is unreachable given FALLBACK_FAILURE_JSON is a
            // static ASCII string with no NULs, but we cannot use
            // `unreachable!()` (which panics) because this whole file's
            // contract is "never panic across FFI". Fall back to the empty
            // (but valid) JSON object as an absolute last resort.
            CString::new("{}").expect("{} has no NUL")
        })
        .into_raw()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;

    /// Helper: convert the raw pointer returned by FFI back into an owned
    /// String and free the pointer.
    unsafe fn consume(ptr: *mut c_char) -> String {
        assert!(
            !ptr.is_null(),
            "FFI returned NULL — never-NULL contract violated"
        );
        let s = unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned();
        unsafe { wn_string_free(ptr) };
        s
    }

    #[test]
    fn null_data_dir_returns_failure_not_null() {
        let logs = CString::new("/tmp/logs").unwrap();
        let svc = CString::new("test.svc").unwrap();

        let ptr = unsafe {
            wn_collect_notifications_after_push(std::ptr::null(), logs.as_ptr(), svc.as_ptr(), 1000)
        };

        let json = unsafe { consume(ptr) };
        let v: serde_json::Value = serde_json::from_str(&json).expect("must be valid JSON");
        assert_eq!(v["status"], "failed");
        assert!(
            v["error"].as_str().unwrap().contains("data_dir"),
            "error mentions data_dir, got: {json}"
        );
    }

    #[test]
    fn null_logs_dir_returns_failure_not_null() {
        let data = CString::new("/tmp/data").unwrap();
        let svc = CString::new("test.svc").unwrap();

        let ptr = unsafe {
            wn_collect_notifications_after_push(data.as_ptr(), std::ptr::null(), svc.as_ptr(), 1000)
        };

        let json = unsafe { consume(ptr) };
        let v: serde_json::Value = serde_json::from_str(&json).expect("must be valid JSON");
        assert_eq!(v["status"], "failed");
        assert!(v["error"].as_str().unwrap().contains("logs_dir"));
    }

    #[test]
    fn null_keyring_service_id_returns_failure_not_null() {
        let data = CString::new("/tmp/data").unwrap();
        let logs = CString::new("/tmp/logs").unwrap();

        let ptr = unsafe {
            wn_collect_notifications_after_push(
                data.as_ptr(),
                logs.as_ptr(),
                std::ptr::null(),
                1000,
            )
        };

        let json = unsafe { consume(ptr) };
        let v: serde_json::Value = serde_json::from_str(&json).expect("must be valid JSON");
        assert_eq!(v["status"], "failed");
        assert!(v["error"].as_str().unwrap().contains("keyring_service_id"));
    }

    #[test]
    fn wn_string_free_null_is_safe() {
        unsafe { wn_string_free(std::ptr::null_mut()) };
    }

    #[test]
    fn string_to_raw_handles_interior_nul() {
        let json_with_nul = String::from("{\"status\":\"failed\",\"error\":\"bad\0value\"}");
        let ptr = string_to_raw(json_with_nul);
        assert!(!ptr.is_null());

        let s = unsafe { CStr::from_ptr(ptr) }
            .to_string_lossy()
            .into_owned();
        unsafe { drop(CString::from_raw(ptr)) };

        // The sanitized version should still be valid JSON.
        assert!(!s.contains('\0'));
        let _: serde_json::Value = serde_json::from_str(&s).expect("sanitized is valid JSON");
    }

    #[test]
    fn fallback_failure_json_is_valid() {
        let v: serde_json::Value = serde_json::from_str(FALLBACK_FAILURE_JSON)
            .expect("FALLBACK_FAILURE_JSON must be valid JSON");
        assert_eq!(v["status"], "failed");
        assert!(v["error"].is_string());
        assert!(v["notifications"].is_array());
        assert!(!FALLBACK_FAILURE_JSON.contains('\0'));
    }
}
