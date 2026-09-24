//! # adjutant-wasm-guest
//!
//! Guest-side helpers for writing an Adjutant WASM plugin. The host side lives
//! in `server/src/wasm.rs`; see that file for the ABI.
//!
//! A plugin implements [`WasmPlugin`] and calls [`export_wasm_plugin!`]:
//!
//! ```ignore
//! use adjutant_wasm_guest::{export_wasm_plugin, host_call, WasmPlugin};
//! use serde_json::{json, Value};
//!
//! struct MyPlugin;
//! impl WasmPlugin for MyPlugin {
//!     fn manifest() -> Value { json!({ "id": "my_plugin", /* … */ }) }
//!     fn handle(req: &Value) -> Value { /* … */ }
//! }
//! export_wasm_plugin!(MyPlugin);
//! ```

use serde_json::{json, Value};

// The one host import: a generic call carrying JSON. See the host module for
// the method set and the result envelope.
#[cfg(target_arch = "wasm32")]
#[link(wasm_import_module = "env")]
unsafe extern "C" {
    fn adjutant_host_call(
        method_ptr: u32,
        method_len: u32,
        payload_ptr: u32,
        payload_len: u32,
        out_ptr: u32,
        out_cap: u32,
    ) -> i32;
}

/// Host stub so the crate also builds for the host target (tests, tooling).
#[cfg(not(target_arch = "wasm32"))]
unsafe fn adjutant_host_call(_: u32, _: u32, _: u32, _: u32, _: u32, _: u32) -> i32 {
    -1
}

/// Call a host service. `method` is e.g. `"db.query"`; `payload` is its JSON
/// object. Returns the `data` field on success, or the host's error message.
pub fn host_call(method: &str, payload: &str) -> Result<Value, String> {
    // Fixed buffer; the host reports `-needed` rather than retrying, so an
    // operation never runs twice.
    let mut out = vec![0u8; 256 * 1024];
    let rc = unsafe {
        adjutant_host_call(
            method.as_ptr() as u32,
            method.len() as u32,
            payload.as_ptr() as u32,
            payload.len() as u32,
            out.as_mut_ptr() as u32,
            out.len() as u32,
        )
    };
    if rc < 0 {
        return Err(format!("host response too large (needs {} bytes)", -rc));
    }
    out.truncate(rc as usize);
    let v: Value = serde_json::from_slice(&out).map_err(|e| format!("bad host JSON: {e}"))?;
    if v["ok"] == json!(true) {
        Ok(v.get("data").cloned().unwrap_or(Value::Null))
    } else {
        Err(v["error"].as_str().unwrap_or("host call failed").to_string())
    }
}

/// Copy `s` into the guest-output buffer at `ptr` with capacity `cap`.
///
/// Returns the byte length, or `-needed` when `cap` is too small (including the
/// `cap == 0` length query). Exposed for [`export_wasm_plugin!`].
pub fn write_out(s: &str, ptr: u32, cap: u32) -> i32 {
    let bytes = s.as_bytes();
    if bytes.len() > cap as usize {
        return -(bytes.len() as i32);
    }
    unsafe {
        std::ptr::copy_nonoverlapping(bytes.as_ptr(), ptr as *mut u8, bytes.len());
    }
    bytes.len() as i32
}

/// The guest-plugin contract.
pub trait WasmPlugin {
    /// The JSON manifest the host reads (`id`, `name`, `version`,
    /// `permissions`, `migrations`, `routes`).
    fn manifest() -> Value;
    /// Handle one request: `req` is the [`crate::host_call`] request object
    /// (`method`, `path`, `params`, `query`, `headers`, `body`, `identity`).
    /// Return `{"status", "headers", "body"}`.
    fn handle(req: &Value) -> Value;
}

/// Export the ABI entry points for a [`WasmPlugin`] implementation.
#[macro_export]
macro_rules! export_wasm_plugin {
    ($t:ty) => {
        #[no_mangle]
        pub extern "C" fn adjutant_alloc(len: u32) -> u32 {
            let mut v = Vec::<u8>::with_capacity(len as usize);
            let ptr = v.as_mut_ptr();
            std::mem::forget(v);
            ptr as u32
        }

        #[no_mangle]
        pub extern "C" fn adjutant_free(ptr: u32, len: u32) {
            unsafe {
                let _ = Vec::from_raw_parts(ptr as *mut u8, 0, len as usize);
            }
        }

        #[no_mangle]
        pub extern "C" fn adjutant_describe(ptr: u32, cap: u32) -> i32 {
            let json = <$t>::manifest().to_string();
            $crate::write_out(&json, ptr, cap)
        }

        #[no_mangle]
        pub extern "C" fn adjutant_handle(
            req_ptr: u32,
            req_len: u32,
            out_ptr: u32,
            out_cap: u32,
        ) -> i32 {
            let req = unsafe { std::slice::from_raw_parts(req_ptr as *const u8, req_len as usize) };
            let value: serde_json::Value =
                serde_json::from_slice(req).unwrap_or(serde_json::Value::Null);
            let resp = <$t>::handle(&value);
            $crate::write_out(&resp.to_string(), out_ptr, out_cap)
        }
    };
}
