//! Linux (glibc) specific support for loading native libraries in isolated linker namespaces
//! via `dlmopen()`.
//!
//! # Why this module exists
//!
//! When native shared libraries that use "initial-exec" thread-local storage (TLS) are loaded
//! at runtime via `dlopen()`, glibc must allocate their TLS out of a fixed-size static TLS
//! block that is reserved at process startup. Once that block is exhausted, `dlopen()` fails
//! with:
//!
//! ```text
//! cannot allocate memory in static TLS block
//! ```
//!
//! This affects libraries built with rustc's private APIs (e.g. `librustc_driver`), because
//! they use initial-exec TLS extensively for performance. Newer versions of rustc tend to use
//! even more TLS, making this problem more likely over time.
//!
//! # The fix: `dlmopen(LM_ID_NEWLM, ...)`
//!
//! `dlmopen` creates a completely new, isolated linker namespace. Each namespace has its own
//! fresh static TLS block, so there is no risk of exhausting the block reserved at process
//! startup. All dependency libraries AND the main plugin library are loaded into the SAME new
//! namespace (by reusing the namespace ID obtained from `dlinfo(RTLD_DI_LMID)` after the
//! first load), so inter-library symbol resolution still works correctly.
//!
//! # Platform scope
//!
//! This module is compiled only on Linux (`#[cfg(target_os = "linux")]`).
//!
//! - **macOS**: uses `dyld`, which allocates TLS dynamically at runtime. The static TLS block
//!   problem does not exist there, so `dlopen` works without issues.
//! - **Windows**: uses `LoadLibrary`, which has no equivalent TLS block constraint.
//! - **Linux with musl libc**: `dlmopen` is a glibc extension and is **NOT available on musl**
//!   (Alpine Linux, musl-based Docker images, static musl builds, etc.). If musl support is
//!   needed in the future, guard this module with
//!   `#[cfg(all(target_os = "linux", not(target_env = "musl")))]` and fall back to the
//!   libloading `dlopen` path for musl targets. Note that musl may still hit the TLS
//!   limitation with `librustc_driver`; on musl the only workaround would be a subprocess
//!   isolation approach.
//!
//! # Cross-namespace memory safety
//!
//! Each dlmopen namespace gets its own copy of `libc`, including its own `malloc`/`free`.
//! Crossing allocator boundaries (allocating in one namespace and freeing in another) would
//! be undefined behavior. The foro plugin ABI avoids this: foro passes a borrowed pointer to
//! the plugin and reads (but never frees) the pointer the plugin returns. No cross-namespace
//! deallocation occurs.

use anyhow::{anyhow, Result};
use std::ffi::{CString, c_void};
use std::os::raw::{c_char, c_int, c_long};
use std::path::PathBuf;

// `Lmid_t` is defined as `long` in glibc's `<dlfcn.h>`.
type Lmid = c_long;

// Sentinel passed to `dlmopen` to request creation of a new linker namespace.
const LM_ID_NEWLM: Lmid = -1;

const RTLD_NOW: c_int = 0x2;
const RTLD_LOCAL: c_int = 0x0;

// `dlinfo` request code that retrieves the `Lmid_t` namespace ID for a handle.
const RTLD_DI_LMID: c_int = 1;

extern "C" {
    fn dlmopen(lmid: Lmid, filename: *const c_char, flags: c_int) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
    fn dlsym(handle: *mut c_void, symbol: *const c_char) -> *mut c_void;
    fn dlinfo(handle: *mut c_void, request: c_int, info: *mut c_void) -> c_int;
    fn dlerror() -> *mut c_char;
}

/// Clears the `dlerror` state and returns any pending error string.
unsafe fn take_dlerror() -> Option<String> {
    let err = dlerror();
    if err.is_null() {
        None
    } else {
        Some(std::ffi::CStr::from_ptr(err).to_string_lossy().into_owned())
    }
}

/// A set of shared library handles all loaded into the same isolated dlmopen namespace.
///
/// The namespace is created on the first [`load_dependency`] / [`load_main`] call and
/// reused for all subsequent loads. This ensures all libraries share symbols within the
/// same TLS space while remaining isolated from the rest of the process.
///
/// [`load_dependency`]: DlmopenNamespace::load_dependency
/// [`load_main`]: DlmopenNamespace::load_main
pub struct DlmopenNamespace {
    /// All handles in load order (dependencies first, main last).
    /// Closed in reverse order on drop.
    handles: Vec<*mut c_void>,
    /// The glibc namespace ID, obtained via `dlinfo(RTLD_DI_LMID)` after the first load.
    /// `None` before any library is loaded.
    namespace_id: Option<Lmid>,
    /// Handle of the main (non-dependency) library. Symbol lookups use this handle.
    main_handle: Option<*mut c_void>,
}

// SAFETY: `dlmopen` handles are per-namespace and safe to move between threads.
// `dlsym` is thread-safe on glibc. After loading, handles are read-only.
unsafe impl Send for DlmopenNamespace {}
unsafe impl Sync for DlmopenNamespace {}

impl DlmopenNamespace {
    pub fn new() -> Self {
        DlmopenNamespace {
            handles: Vec::new(),
            namespace_id: None,
            main_handle: None,
        }
    }

    /// Load a dependency library into this namespace.
    ///
    /// Call this for every dependency in topological load order, before calling
    /// [`load_main`]. The dependency's handle is kept alive for the lifetime of
    /// this namespace to prevent premature unloading.
    ///
    /// [`load_main`]: DlmopenNamespace::load_main
    pub fn load_dependency(&mut self, path: &PathBuf) -> Result<()> {
        let handle = self.open_into_namespace(path)?;
        self.handles.push(handle);
        Ok(())
    }

    /// Load the main plugin library into this namespace.
    ///
    /// Symbol lookups via [`get_symbol`] use this handle.
    ///
    /// [`get_symbol`]: DlmopenNamespace::get_symbol
    pub fn load_main(&mut self, path: &PathBuf) -> Result<()> {
        let handle = self.open_into_namespace(path)?;
        self.handles.push(handle);
        self.main_handle = Some(handle);
        Ok(())
    }

    /// Open a library into this namespace. Creates the namespace on the first call.
    fn open_into_namespace(&mut self, path: &PathBuf) -> Result<*mut c_void> {
        let path_str = path
            .to_str()
            .ok_or_else(|| anyhow!("Path contains invalid UTF-8: {}", path.display()))?;
        let c_path =
            CString::new(path_str).map_err(|e| anyhow!("Path contains null byte: {}", e))?;

        let lmid = self.namespace_id.unwrap_or(LM_ID_NEWLM);

        // Clear any leftover error before the call.
        unsafe { dlerror() };

        let handle = unsafe { dlmopen(lmid, c_path.as_ptr(), RTLD_NOW | RTLD_LOCAL) };

        if handle.is_null() {
            let err = unsafe { take_dlerror() }.unwrap_or_else(|| "(unknown error)".to_string());
            return Err(anyhow!("dlmopen failed for {}: {}", path.display(), err));
        }

        // On the first successful load, retrieve and cache the namespace ID so that all
        // subsequent loads reuse the same namespace rather than creating new ones.
        if self.namespace_id.is_none() {
            let mut ns_id: Lmid = 0;
            let ret = unsafe {
                dlinfo(
                    handle,
                    RTLD_DI_LMID,
                    &mut ns_id as *mut Lmid as *mut c_void,
                )
            };
            if ret != 0 {
                let err =
                    unsafe { take_dlerror() }.unwrap_or_else(|| "(unknown error)".to_string());
                // Non-fatal: log a warning and leave namespace_id as None. Subsequent
                // libraries will be loaded with LM_ID_NEWLM, creating separate namespaces.
                // This will likely cause symbol resolution failures for the plugin, but it
                // is safer than panicking at this point.
                log::warn!(
                    "dlinfo(RTLD_DI_LMID) failed: {}. \
                     Subsequent libraries may fail to resolve symbols.",
                    err
                );
            } else {
                self.namespace_id = Some(ns_id);
            }
        }

        Ok(handle)
    }

    /// Look up a symbol by name in the main library handle.
    ///
    /// Returns the raw symbol address as a `*mut c_void`. The caller is responsible
    /// for casting it to the appropriate function pointer type.
    pub fn get_symbol(&self, name: &str) -> Result<*mut c_void> {
        let main_handle = self
            .main_handle
            .ok_or_else(|| anyhow!("No main library loaded in this namespace"))?;

        let c_name =
            CString::new(name).map_err(|e| anyhow!("Symbol name contains null byte: {}", e))?;

        // Clear any leftover error before the call.
        unsafe { dlerror() };

        let sym = unsafe { dlsym(main_handle, c_name.as_ptr()) };

        if sym.is_null() {
            let err = unsafe { take_dlerror() }
                .unwrap_or_else(|| format!("symbol '{}' not found", name));
            return Err(anyhow!("dlsym failed for '{}': {}", name, err));
        }

        Ok(sym)
    }
}

impl Default for DlmopenNamespace {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DlmopenNamespace {
    fn drop(&mut self) {
        // Close handles in reverse load order so that the main library is closed before
        // its dependencies, matching the expected unload sequence.
        for handle in self.handles.drain(..).rev() {
            unsafe {
                dlclose(handle);
            }
        }
    }
}
