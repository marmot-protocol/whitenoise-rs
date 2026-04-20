//! C ABI entrypoints for native iOS background push handling.
//!
//! Callable from Swift via the static library without requiring Flutter or FRB.
//!
//! # Design — why callback + length-prefixed bytes
//!
//! The FFI surface is designed for safety across the Rust/Swift boundary:
//!
//! * **Length-prefixed byte slices on input.** All string inputs are passed as
//!   `(ptr, len)` pairs rather than NUL-terminated C strings. This avoids
//!   unbounded scanning into foreign memory and keeps inputs binary-safe.
//!
//! * **Callback for output.** JSON results are delivered via a callback
//!   invoked while Rust still owns the bytes. The caller can read (and copy)
//!   during the callback; after the callback returns, the buffer is freed
//!   deterministically by Rust. There is no `into_raw` / `from_raw` ownership
//!   handoff across the boundary, so use-after-free and double-free are
//!   structurally impossible.
//!
//! # Contract guarantees
//!
//! * [`wn_collect_notifications_after_push`] **never panics across the FFI
//!   boundary on outbound Rust→Swift unwinds** — all internal panics are
//!   caught and surfaced as a failure JSON callback invocation.
//! * It **always invokes the callback exactly once** before returning,
//!   regardless of init/runtime/serialization failures.
//! * The JSON payload delivered to the callback **always** conforms to:
//!   ```json
//!   {
//!     "status": "new_data" | "no_data" | "failed",
//!     "notifications": [ ... ],
//!     "error": "..."   // only present when status is "failed"
//!   }
//!   ```
//!
//! # Caller responsibilities
//!
//! The caller must ensure the supplied [`WnJsonCallback`] does not panic or
//! unwind. Panicking across an `extern "C"` boundary is undefined behavior in
//! Rust and in practice will abort the process. On Darwin, Swift runtime
//! traps (`fatalError`, force-unwrap nil, index-out-of-range) terminate via
//! `abort()` rather than a structured unwind, so they cannot damage Rust's
//! state, but they will kill the process immediately. Write callbacks that
//! only copy bytes and return — do no work that could trap.

use std::ffi::c_void;
use std::panic::AssertUnwindSafe;
use std::slice;
use std::sync::OnceLock;
use std::time::Duration;

use crate::whitenoise::WhitenoiseConfig;
use crate::whitenoise::background_notifications::{
    BackgroundNotificationResult, collect_notifications_after_push,
};

/// Static fallback JSON used when dynamic serialization fails.
/// Guaranteed to be valid JSON conforming to the documented shape.
const FALLBACK_FAILURE_JSON: &str =
    r#"{"status":"failed","notifications":[],"error":"internal error: failed to build response"}"#;

/// Static failure JSON used specifically when the main body's `catch_unwind`
/// catches an internal panic. Distinct from [`FALLBACK_FAILURE_JSON`] so that
/// debugging can distinguish a panic origin from a serialization-failure
/// origin. Both are intentionally hardcoded so they cannot themselves fail.
const PANIC_FAILURE_JSON: &str = r#"{"status":"failed","notifications":[],"error":"internal panic in wn_collect_notifications_after_push"}"#;

/// Number of worker threads for [`FFI_RUNTIME`]. Two is the minimum that
/// guarantees the event processor and the drain loop can make concurrent
/// progress: one worker drives `drain_notifications` (blocked on the
/// broadcast receiver), the other drives the spawned event processor that
/// feeds it. More than two would be wasteful for this workload.
const FFI_RUNTIME_WORKERS: usize = 2;

/// Process-lifetime Tokio runtime used by [`wn_collect_notifications_after_push`].
///
/// Two reasons this must be process-lifetime *and* multi-threaded:
///
/// 1. **Outlive the FFI call.** `initialize_whitenoise` spawns long-lived
///    tasks (event processor, scheduled tasks, discovery sync worker) onto
///    whichever runtime is active when `block_on` is running. If we built
///    a fresh runtime per FFI call and dropped it on return, those tasks
///    would be aborted, leaving the `GLOBAL_WHITENOISE` singleton populated
///    but functionally dead: the next FFI call would fast-path through
///    `ensure_initialized` yet silently produce `no_data` because no event
///    processor is running.
/// 2. **Multi-threaded for concurrency.** A `current_thread` runtime can in
///    principle interleave tasks during `await` points, but it is fragile:
///    any synchronous CPU work in the event-processor / decryption path
///    would stall the drain loop, and any future code that does a small
///    blocking call inline would deadlock. A multi-thread runtime sized for
///    this workload (event processor on one worker, drain on another)
///    eliminates that class of concern entirely. iOS background processes
///    can comfortably afford two worker threads.
static FFI_RUNTIME: OnceLock<tokio::runtime::Runtime> = OnceLock::new();

/// Get or lazily initialize the process-lifetime Tokio runtime.
///
/// Returns `None` if the runtime could not be constructed (extremely rare —
/// typically only happens if OS thread/fd allocation fails). Callers should
/// surface this as a failure JSON via the callback.
fn ffi_runtime() -> Option<&'static tokio::runtime::Runtime> {
    if let Some(rt) = FFI_RUNTIME.get() {
        return Some(rt);
    }
    match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(FFI_RUNTIME_WORKERS)
        .enable_all()
        .thread_name("wn-ffi")
        .build()
    {
        Ok(rt) => {
            // If another caller raced us to init, `set` returns Err with the
            // losing value (which is then dropped). Either way, `get` returns
            // the winner.
            let _ = FFI_RUNTIME.set(rt);
            FFI_RUNTIME.get()
        }
        Err(e) => {
            tracing::error!(
                target: "whitenoise::ffi",
                "Failed to build FFI Tokio runtime: {}",
                e
            );
            None
        }
    }
}

/// Callback invoked with a JSON payload.
///
/// # Parameters
///
/// * `json_ptr` — Pointer to a UTF-8 byte buffer. Not NUL-terminated.
///   **Valid only for the duration of the callback invocation.** Must not
///   be retained past the callback's return.
/// * `json_len` — Length of the buffer in bytes.
/// * `user_data` — Opaque context pointer passed through from the caller.
///
/// Callers must copy any data they wish to retain before returning.
pub type WnJsonCallback =
    extern "C" fn(json_ptr: *const u8, json_len: usize, user_data: *mut c_void);

/// Collect notification updates after an iOS silent push wake.
///
/// Initializes Whitenoise if needed, refreshes relay subscriptions, and
/// collects any notification updates that arrive within the time window.
/// The resulting JSON is delivered to `callback` exactly once before this
/// function returns.
///
/// # Parameters
///
/// * `data_dir_ptr`, `data_dir_len` — UTF-8 bytes for the app data directory.
/// * `logs_dir_ptr`, `logs_dir_len` — UTF-8 bytes for the logs directory.
/// * `keyring_service_id_ptr`, `keyring_service_id_len` — UTF-8 bytes for the
///   keyring service identifier.
/// * `max_wait_ms` — Hard deadline for collection in milliseconds. `u32` is
///   intentional: it matches the C `uint32_t` convention for short timeouts
///   and gives a maximum representable wait of ~49 days, far above the
///   internal `IOS_MAX_WAIT` clamp (25 s). A wider integer would suggest the
///   value is unbounded in practice, which it is not.
/// * `callback` — Function invoked with the JSON result. See
///   [`WnJsonCallback`] for lifetime rules.
/// * `user_data` — Opaque context passed through to `callback`. Not
///   interpreted by Rust.
///
/// # Safety
///
/// * `data_dir_ptr`, `logs_dir_ptr`, and `keyring_service_id_ptr` must each
///   either be null or point to a valid readable buffer of at least the
///   corresponding `*_len` bytes. Null pointers with non-zero length are
///   treated as an error (reported via the callback).
/// * `callback` must be a valid function pointer safe to call with
///   `extern "C"` calling convention.
/// * `user_data` is treated as opaque; Rust neither reads nor writes through
///   it. It is the caller's responsibility to ensure any pointer-derived
///   `user_data` remains valid for the duration of this call.
/// * The buffer passed to `callback` is only valid until the callback
///   returns. Callers must copy any bytes they wish to retain.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wn_collect_notifications_after_push(
    data_dir_ptr: *const u8,
    data_dir_len: usize,
    logs_dir_ptr: *const u8,
    logs_dir_len: usize,
    keyring_service_id_ptr: *const u8,
    keyring_service_id_len: usize,
    max_wait_ms: u32,
    callback: WnJsonCallback,
    user_data: *mut c_void,
) {
    // Run the body under catch_unwind so that no internal Rust panic can
    // cross the FFI boundary as an unwind. The closure produces a JSON
    // String which is then delivered to the callback below.
    let json = std::panic::catch_unwind(AssertUnwindSafe(|| {
        // Validate and decode each input. Any failure produces a failure
        // JSON payload without proceeding.
        let data_dir = match unsafe { slice_to_string(data_dir_ptr, data_dir_len, "data_dir") } {
            Ok(s) => s,
            Err(json) => return json,
        };
        let logs_dir = match unsafe { slice_to_string(logs_dir_ptr, logs_dir_len, "logs_dir") } {
            Ok(s) => s,
            Err(json) => return json,
        };
        let keyring_service_id = match unsafe {
            slice_to_string(
                keyring_service_id_ptr,
                keyring_service_id_len,
                "keyring_service_id",
            )
        } {
            Ok(s) => s,
            Err(json) => return json,
        };

        let config = WhitenoiseConfig::new(
            std::path::Path::new(&data_dir),
            std::path::Path::new(&logs_dir),
            &keyring_service_id,
        );
        let max_wait = Duration::from_millis(u64::from(max_wait_ms));

        // Drive the async work on a process-lifetime runtime. See
        // [`FFI_RUNTIME`] for why this cannot be a per-call runtime: the
        // spawned tasks inside `initialize_whitenoise` must outlive the
        // `block_on` that spawned them.
        let rt = match ffi_runtime() {
            Some(rt) => rt,
            None => {
                return result_to_json(BackgroundNotificationResult::failed(
                    "failed to create tokio runtime".to_string(),
                ));
            }
        };

        let result = rt.block_on(collect_notifications_after_push(config, max_wait));
        result_to_json(result)
    }))
    .unwrap_or_else(|_| {
        // Internal panic escaped. Fall back to a static failure JSON; the
        // callback is still invoked exactly once below.
        PANIC_FAILURE_JSON.to_string()
    });

    // Invoke the callback with the JSON bytes. The buffer lives on Rust's
    // stack frame for the duration of this call and is dropped when this
    // function returns, so the pointer is valid exactly for the callback's
    // execution. Per the module-level docs, the callback must not panic or
    // trap — doing so is UB across `extern "C"` and will abort the process.
    let bytes = json.as_bytes();
    callback(bytes.as_ptr(), bytes.len(), user_data);
}

/// Convert a `(ptr, len)` byte slice into an owned `String`, returning a
/// failure JSON payload on null-with-length, invalid UTF-8, or other
/// unrecoverable condition.
///
/// A null pointer with `len == 0` is treated as an empty string — this is
/// what Swift's `UnsafePointer` APIs produce for empty strings and is less
/// brittle than rejecting it outright.
///
/// # Safety
///
/// Caller must ensure that if `ptr` is non-null, it points to a valid readable
/// buffer of at least `len` bytes.
unsafe fn slice_to_string(ptr: *const u8, len: usize, field: &str) -> Result<String, String> {
    if ptr.is_null() {
        if len == 0 {
            return Ok(String::new());
        }
        return Err(result_to_json(BackgroundNotificationResult::failed(
            format!("null pointer for `{field}` with non-zero length"),
        )));
    }
    // SAFETY: ptr is non-null; caller guarantees `len` bytes are valid.
    let bytes = unsafe { slice::from_raw_parts(ptr, len) };
    match std::str::from_utf8(bytes) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    /// Thread-safe collector for callback payloads.
    ///
    /// Swift passes its own context pointer; tests use this to capture the
    /// bytes delivered to the callback.
    #[derive(Default)]
    struct Capture {
        payloads: Mutex<Vec<Vec<u8>>>,
    }

    impl Capture {
        fn new() -> Self {
            Self::default()
        }

        fn take_only(&self) -> Vec<u8> {
            let mut payloads = self.payloads.lock().unwrap();
            assert_eq!(
                payloads.len(),
                1,
                "callback must be invoked exactly once; got {} invocations",
                payloads.len()
            );
            payloads.pop().unwrap()
        }

        fn invocation_count(&self) -> usize {
            self.payloads.lock().unwrap().len()
        }
    }

    extern "C" fn capture_callback(ptr: *const u8, len: usize, user_data: *mut c_void) {
        // SAFETY: tests pass a `&Capture` as user_data.
        let capture = unsafe { &*(user_data as *const Capture) };
        let bytes = if ptr.is_null() || len == 0 {
            Vec::new()
        } else {
            // SAFETY: the FFI function guarantees `ptr`/`len` validity for the
            // duration of this callback.
            let slice = unsafe { slice::from_raw_parts(ptr, len) };
            slice.to_vec()
        };
        capture.payloads.lock().unwrap().push(bytes);
    }

    /// Returns the (ptr, len) pair representing a null pointer with a
    /// caller-supplied non-zero length — the input shape we want to reject.
    fn null_with_len(len: usize) -> (*const u8, usize) {
        (std::ptr::null(), len)
    }

    /// Returns the (ptr, len) pair for a valid string view.
    fn from_str(s: &str) -> (*const u8, usize) {
        (s.as_ptr(), s.len())
    }

    /// Run the FFI once with explicit (ptr, len) pairs for each input and
    /// return the parsed JSON payload delivered to the callback.
    fn run_ffi(
        data_dir: (*const u8, usize),
        logs_dir: (*const u8, usize),
        svc: (*const u8, usize),
    ) -> serde_json::Value {
        let capture = Capture::new();
        let user_data = &capture as *const Capture as *mut c_void;
        unsafe {
            wn_collect_notifications_after_push(
                data_dir.0,
                data_dir.1,
                logs_dir.0,
                logs_dir.1,
                svc.0,
                svc.1,
                1000,
                capture_callback,
                user_data,
            );
        }
        let bytes = capture.take_only();
        let s = std::str::from_utf8(&bytes).expect("callback payload must be valid UTF-8");
        serde_json::from_str(s).expect("callback payload must be valid JSON")
    }

    #[test]
    fn null_data_dir_ptr_with_len_reports_failure_via_callback() {
        let v = run_ffi(
            null_with_len(10),
            from_str("/tmp/logs"),
            from_str("test.svc"),
        );
        assert_eq!(v["status"], "failed");
        assert!(
            v["error"].as_str().unwrap().contains("data_dir"),
            "error mentions data_dir, got: {v}"
        );
    }

    #[test]
    fn null_logs_dir_ptr_with_len_reports_failure_via_callback() {
        let v = run_ffi(
            from_str("/tmp/data"),
            null_with_len(10),
            from_str("test.svc"),
        );
        assert_eq!(v["status"], "failed");
        assert!(v["error"].as_str().unwrap().contains("logs_dir"));
    }

    #[test]
    fn null_keyring_service_id_ptr_with_len_reports_failure_via_callback() {
        let v = run_ffi(
            from_str("/tmp/data"),
            from_str("/tmp/logs"),
            null_with_len(10),
        );
        assert_eq!(v["status"], "failed");
        assert!(v["error"].as_str().unwrap().contains("keyring_service_id"));
    }

    #[test]
    fn invalid_utf8_input_reports_failure_via_callback() {
        // 0xFF is not valid UTF-8.
        let bad: [u8; 3] = [b'/', 0xFF, b'x'];
        let logs = "/tmp/logs";
        let svc = "test.svc";

        let capture = Capture::new();
        let user_data = &capture as *const Capture as *mut c_void;

        unsafe {
            wn_collect_notifications_after_push(
                bad.as_ptr(),
                bad.len(),
                logs.as_ptr(),
                logs.len(),
                svc.as_ptr(),
                svc.len(),
                1000,
                capture_callback,
                user_data,
            );
        }

        let bytes = capture.take_only();
        let s = std::str::from_utf8(&bytes).expect("payload must be valid UTF-8");
        let v: serde_json::Value = serde_json::from_str(s).expect("valid JSON");
        assert_eq!(v["status"], "failed");
        assert!(v["error"].as_str().unwrap().contains("UTF-8"));
        assert!(v["error"].as_str().unwrap().contains("data_dir"));
    }

    #[test]
    fn callback_invoked_exactly_once_even_on_failure() {
        // Several failure paths — null pointer, invalid UTF-8, etc. — must
        // each invoke the callback exactly once. We already assert this in
        // `Capture::take_only`; this test makes the contract explicit.
        let logs = "/tmp/logs";
        let svc = "test.svc";

        let capture = Capture::new();
        let user_data = &capture as *const Capture as *mut c_void;

        unsafe {
            wn_collect_notifications_after_push(
                std::ptr::null(),
                10,
                logs.as_ptr(),
                logs.len(),
                svc.as_ptr(),
                svc.len(),
                1000,
                capture_callback,
                user_data,
            );
        }

        assert_eq!(capture.invocation_count(), 1);
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

    #[test]
    fn null_ptr_with_zero_len_is_empty_string_not_failure() {
        // Swift may produce null pointer + len=0 for empty strings. Our code
        // treats that as an empty string rather than an error; the actual
        // config will then fail later (empty keyring id rejected) or succeed
        // (empty path, though practically unlikely). Here we just verify the
        // slice-to-string helper's handling, exercised via an empty
        // keyring_service_id where Whitenoise init will reject it explicitly
        // with a different error. Use this to confirm the ptr=null+len=0
        // branch does NOT itself surface as a null-pointer error.

        // Using a valid-but-nonexistent data dir so init will fail quickly;
        // the important assertion is that the failure is NOT about null
        // pointers.
        let logs = "/tmp";
        let svc = "";
        let capture = Capture::new();
        let user_data = &capture as *const Capture as *mut c_void;
        unsafe {
            wn_collect_notifications_after_push(
                std::ptr::null(), // data_dir ptr is null...
                0,                // ...but len is 0, so treated as empty
                logs.as_ptr(),
                logs.len(),
                svc.as_ptr(),
                svc.len(),
                1000,
                capture_callback,
                user_data,
            );
        }

        let bytes = capture.take_only();
        let s = std::str::from_utf8(&bytes).expect("payload must be valid UTF-8");
        let v: serde_json::Value = serde_json::from_str(s).expect("valid JSON");
        // Whatever the outcome, it should NOT be a null-pointer error — the
        // empty string case is accepted and init continues.
        if v["status"] == "failed" {
            let err = v["error"].as_str().unwrap_or("");
            assert!(
                !err.contains("null pointer"),
                "empty string via (null, 0) must not be treated as null pointer, got: {err}"
            );
        }
    }
}
