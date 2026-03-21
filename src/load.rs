use crate::resolve::{resolve, ResolveError};
use crate::type_utils::{Caller, IOToFn};
use anyhow::{anyhow, Result};
#[cfg(target_os = "linux")]
use crate::linux_dlmopen::DlmopenNamespace;
// On Linux, only Symbol is needed from libloading (for the LLFunction variant that still exists
// in the enum but is never instantiated on Linux).
#[cfg(all(unix, not(target_os = "linux")))]
use libloading::os::unix::{Library as LLNativeLibrary, RTLD_LOCAL, RTLD_NOW};
#[cfg(unix)]
use libloading::os::unix::Symbol;
#[cfg(windows)]
use libloading::os::windows::{Library as LLNativeLibrary, Symbol};
#[cfg(windows)]
use crate::fs_utils::get_available_drives;
use log::{debug, trace};
use std::fs;
use std::marker::PhantomData;
use std::ops::Deref;
use std::path::PathBuf;
use url::Url;
use wasmtime::{Config, Engine, Instance as WasmInstance, Linker, Module, Store, TypedFunc};
use wasmtime_wasi::preview1::WasiP1Ctx;
use wasmtime_wasi::{preview1, DirPerms, FilePerms, WasiCtxBuilder};
use crate::target_triple::THIS_PLATFORM;

/// It represents a callable function loaded from a library,
/// abstracting both native and WASM libraries.
///
/// The type parameters `Args` and `Res` define the function signature,
/// and `IOToFn` provides the conversion logic.
pub enum Function<Args, Res>
where
    (Args, Res): IOToFn,
{
    LLFunction(Symbol<<(Args, Res) as IOToFn>::Output>),
    WasmFunction(TypedFunc<Args, Res>),
    /// A native function loaded via `dlmopen` on Linux.
    ///
    /// The function address is stored as an opaque `unsafe extern "C" fn()` pointer.
    /// The concrete signature is recovered at call time via `transmute_copy`, which avoids
    /// the compile-time size check that `transmute` cannot perform for generic associated types.
    ///
    /// # Safety
    ///
    /// The stored pointer must be the address of a function whose actual ABI matches
    /// `<(Args, Res) as IOToFn>::Output`. Callers must ensure this when constructing
    /// `DlmopenFn`.
    #[cfg(target_os = "linux")]
    DlmopenFn(unsafe extern "C" fn(), PhantomData<(Args, Res)>),
}

impl<Args, Res> Function<Args, Res>
where
    Args: wasmtime::WasmParams,
    Res: wasmtime::WasmResults,
    (Args, Res): IOToFn,
    Args: Caller<Args, Res>,
{
    /// Dispatches function calls to either native or wasm implementations based on the variant.
    pub fn call(&self, library: &mut Library, args: Args) -> Res {
        match &self {
            Function::LLFunction(symbol) => unsafe {
                let a = symbol.deref();
                <Args as Caller<Args, Res>>::call(args, a)
            },
            Function::WasmFunction(func) => {
                let Library::WasmLibrary(WasmLibrary { store, .. }) = library else {
                    panic!("Wasm function cannot be called without Wasm library");
                };
                <TypedFunc<Args, Res>>::call(func, store, args).unwrap()
            }
            #[cfg(target_os = "linux")]
            Function::DlmopenFn(fn_ptr, _) => unsafe {
                // Reinterpret the opaque fn pointer as the concrete function signature.
                // Both the stored pointer and the target type are function pointers of the
                // same pointer width (8 bytes on all supported platforms). We use
                // `transmute_copy` rather than `transmute` because `transmute` cannot
                // statically verify size equality when the target is a generic associated
                // type (`<(Args, Res) as IOToFn>::Output`). `transmute_copy` copies exactly
                // `size_of::<Dst>()` bytes from the source reference, which is correct here.
                let f: <(Args, Res) as IOToFn>::Output = std::mem::transmute_copy(fn_ptr);
                <Args as Caller<Args, Res>>::call(args, &f)
            },
        }
    }
}

/// A struct that stores OS-native DLLs.
///
/// On Linux (glibc), libraries are loaded via `dlmopen` into an isolated linker namespace
/// to avoid the "cannot allocate memory in static TLS block" error that occurs when
/// `dlopen` is used with libraries that make heavy use of initial-exec TLS (such as
/// `librustc_driver`). See the `linux_dlmopen` module for a full explanation.
///
/// On other platforms (macOS, Windows), libloading is used directly.
///
/// By owning not only the library itself but also its dependencies,
/// this struct prevents the library and its dependent parts from being unloaded prematurely.
pub struct NativeLibrary {
    /// Linux: isolated dlmopen namespace holding all libraries (deps + main).
    #[cfg(target_os = "linux")]
    pub namespace: DlmopenNamespace,

    /// Non-Linux: the main library handle from libloading.
    #[cfg(not(target_os = "linux"))]
    pub raw_library: LLNativeLibrary,

    /// Non-Linux: dependency library handles kept alive to prevent premature unloading.
    #[cfg(not(target_os = "linux"))]
    pub raw_dependencies: Vec<LLNativeLibrary>,
}

/// A struct that encapsulates a wasmtime instance and a context for WASI operations.
pub struct WasmLibrary {
    pub instance: WasmInstance,
    pub store: Store<WasiP1Ctx>,
}

/// An interface that abstracts both native libraries and WASM libraries.
pub enum Library {
    NativeLibrary(NativeLibrary),
    WasmLibrary(WasmLibrary),
}

impl Library {
    #[cfg(not(target_os = "linux"))]
    pub(crate) fn new_native_library(
        raw_library: LLNativeLibrary,
        raw_dependencies: Vec<LLNativeLibrary>,
    ) -> Self {
        Library::NativeLibrary(NativeLibrary {
            raw_library,
            raw_dependencies,
        })
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn new_native_library_dlmopen(namespace: DlmopenNamespace) -> Self {
        Library::NativeLibrary(NativeLibrary { namespace })
    }

    pub(crate) fn new_wasm_library(instance: WasmInstance, store: Store<WasiP1Ctx>) -> Self {
        Library::WasmLibrary(WasmLibrary { instance, store })
    }

    /// Retrieves a function from the library with type-safe bindings.
    pub fn get_function<Args, Res>(&mut self, name: &str) -> Result<Function<Args, Res>>
    where
        Args: wasmtime::WasmParams,
        Res: wasmtime::WasmResults,
        (Args, Res): IOToFn,
        Args: Caller<Args, Res>,
    {
        match self {
            #[cfg(not(target_os = "linux"))]
            Library::NativeLibrary(NativeLibrary {
                raw_library: lib, ..
            }) => {
                let symbol: Symbol<<(Args, Res) as IOToFn>::Output> =
                    unsafe { lib.get(name.as_bytes())? };
                Ok(Function::LLFunction(symbol))
            }
            #[cfg(target_os = "linux")]
            Library::NativeLibrary(NativeLibrary { namespace, .. }) => {
                let sym = namespace.get_symbol(name)?;
                // SAFETY: `sym` is the address of `name` in the plugin library, obtained from
                // `dlsym`. We store it as an opaque fn pointer and reinterpret at call time.
                // Converting a `*mut c_void` from `dlsym` to a function pointer is the
                // standard POSIX pattern (POSIX 2008 XSI extension) and safe on Linux.
                let raw: unsafe extern "C" fn() = unsafe { std::mem::transmute(sym) };
                Ok(Function::DlmopenFn(raw, PhantomData))
            }
            Library::WasmLibrary(WasmLibrary { instance, store }) => {
                let func = instance.get_typed_func::<Args, Res>(store, name)?;
                Ok(Function::WasmFunction(func))
            }
        }
    }
}

fn is_wasm(platform: &str) -> bool {
    platform.contains("wasm")
}

#[cfg(unix)]
fn pre_open_all(wasi_ctx_builder: &mut WasiCtxBuilder) -> Result<()> {
    wasi_ctx_builder.preopened_dir("/", "/", DirPerms::all(), FilePerms::all())?;

    Ok(())
}

#[cfg(windows)]
fn pre_open_all(wasi_ctx_builder: &mut WasiCtxBuilder) -> Result<()> {
    // Note that it cannot handle, for example, drives connected after the context has been created.
    // Some alternative solution is needed for this.
    for drive in get_available_drives() {
        wasi_ctx_builder.preopened_dir(
            format!("{}:\\", drive.to_uppercase()),
            format!("/{}", drive.to_lowercase()),
            DirPerms::all(),
            FilePerms::all(),
        )?;
    }

    Ok(())
}

/// Loads a wasm library with WASI support, including module caching for performance.
pub fn load_with_wasm(url: &Url, work_dir: &PathBuf, platform: &str) -> Result<Library> {
    debug!("toplevel-load with {}: {}", platform, url);

    let (base_info, dependency_load_order_paths) = resolve(url, work_dir, platform)?;

    // Basic wasm file cannot include dependencies.
    // Note: Wasm component can include dependencies maybe.
    if !dependency_load_order_paths.is_empty() {
        return Err(anyhow!("Wasm file cannot include dependencies"));
    }

    let mut config = Config::default();
    // See https://github.com/bytecodealliance/wasmtime/issues/8897
    #[cfg(unix)]
    config.native_unwind_info(false);

    let engine = Engine::new(&config)?;

    let cache_path = base_info.wasm_module_cache_path();

    // Use cached module if available.
    let module = if cache_path.exists() {
        debug!(
            "{}: loading from cache: {}",
            base_info.name,
            cache_path.display()
        );

        let module;
        unsafe {
            module = Module::deserialize_file(&engine, &cache_path)?;
        }

        module
    } else {
        debug!(
            "{}: manual loading: {}",
            base_info.name,
            base_info.path.display()
        );

        let wasm_bin = fs::read(&base_info.path)?;
        let module = Module::from_binary(&engine, wasm_bin.as_slice())?;

        let cache_bin = module.serialize()?;

        trace!("serializing to cache: {}", cache_path.display());

        fs::create_dir_all(cache_path.parent().unwrap())?;
        fs::write(&cache_path, cache_bin)?;

        module
    };

    let mut linker = Linker::new(&engine);

    // Set up wasi environment with full system access.
    //
    // One possible way to ensure security would be
    // to limit the host-side permissions accessible from the WASI environment.
    // However, since dllpack can load native libraries,
    // such restrictions would not be very meaningful in practice.
    //
    // Therefore, we do not plan to offer such an option.
    preview1::add_to_linker_sync(&mut linker, |t| t)?;
    let pre = linker.instantiate_pre(&module)?;

    let mut wasi_ctx_builder = WasiCtxBuilder::new();

    wasi_ctx_builder.inherit_env();
    wasi_ctx_builder.inherit_stdio();

    pre_open_all(&mut wasi_ctx_builder)?;

    let wasi_ctx = wasi_ctx_builder.build_p1();

    let mut store = Store::new(&engine, wasi_ctx);
    let instance = pre.instantiate(&mut store)?;

    Ok(Library::new_wasm_library(instance, store))
}

/// Loads a native library via libloading (`dlopen` / `LoadLibrary`).
///
/// Not used on Linux; see `linux_dlmopen` for the Linux loading path.
#[cfg(all(unix, not(target_os = "linux")))]
unsafe fn libloading_load(path: &PathBuf) -> Result<LLNativeLibrary> {
    LLNativeLibrary::open(Some(path), RTLD_NOW | RTLD_LOCAL).map_err(|e| e.into())
}

#[cfg(windows)]
unsafe fn libloading_load(path: &PathBuf) -> Result<LLNativeLibrary> {
    LLNativeLibrary::new(path).map_err(|e| e.into())
}

/// Downloads the dllpack from the specified URL and loads it for the specified platform.
/// Both the download and loading processes are cached.
pub fn load_with_platform(url: &Url, work_dir: &PathBuf, platform: &str) -> Result<Library> {
    if is_wasm(platform) {
        return load_with_wasm(url, work_dir, platform);
    }

    debug!("toplevel-load with {}: {}", platform, url);

    let (base_info, dependency_load_order_paths) = resolve(url, work_dir, platform)?;

    // On Linux (glibc), load all libraries into an isolated dlmopen namespace to avoid
    // the "cannot allocate memory in static TLS block" error that occurs when dlopen is
    // used with heavy initial-exec TLS libraries such as librustc_driver. See the
    // linux_dlmopen module for a detailed explanation.
    //
    // Note: this path is NOT taken on musl Linux because dlmopen is a glibc extension.
    // If musl support is added later, gate this block with:
    //   #[cfg(all(target_os = "linux", not(target_env = "musl")))]
    // and provide a libloading fallback for musl. See linux_dlmopen module docs.
    #[cfg(target_os = "linux")]
    {
        let mut namespace = DlmopenNamespace::new();

        for d in &dependency_load_order_paths {
            trace!("loading dependency into dlmopen namespace: {}", d.url);
            namespace.load_dependency(&d.path)?;
        }

        trace!(
            "loading base library into dlmopen namespace: {}",
            base_info.url
        );
        namespace.load_main(&base_info.path)?;

        return Ok(Library::new_native_library_dlmopen(namespace));
    }

    // On non-Linux platforms (macOS, Windows), use libloading.
    // macOS does not have the static TLS block problem (dyld allocates TLS dynamically).
    // Windows uses LoadLibrary which has no equivalent constraint.
    #[cfg(not(target_os = "linux"))]
    {
        let mut dependency_libs = Vec::new();

        // Load dependencies in order before the main library.
        for d in dependency_load_order_paths {
            trace!("loading dependency: {}", d.url);
            let lib = unsafe { libloading_load(&d.path)? };
            dependency_libs.push(lib);
        }

        trace!("loading base library: {}", base_info.url);
        let lib = unsafe { libloading_load(&base_info.path)? };

        Ok(Library::new_native_library(lib, dependency_libs))
    }
}

/// The entry point for library loading that first attempts native loading
/// and falls back to WASM if necessary.
/// This provides transparent cross-platform support with WASM as a fallback.
pub fn load(url: &Url, work_dir: &PathBuf) -> Result<Library> {
    let this_platform = THIS_PLATFORM;
    let with_this_platform = load_with_platform(url, work_dir, this_platform);

    let res = match with_this_platform {
        Ok(v) => v,
        Err(e) => {
            if let Some(m) = e.downcast_ref::<ResolveError>() {
                debug!("Failed to load with this platform: {}", m);

                load_with_wasm(url, work_dir, "wasm32-wasip1")?
            } else {
                return Err(e);
            }
        }
    };

    debug!("loaded: {}", url);

    Ok(res)
}
