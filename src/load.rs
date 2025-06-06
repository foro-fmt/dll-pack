use crate::fs_utils::get_available_drives;
use crate::resolve::{resolve, ResolveError};
use crate::type_utils::{Caller, IOToFn};
use anyhow::{anyhow, Result};
#[cfg(unix)]
use libloading::os::unix::{
    Library as LLNativeLibrary, // LL means libloading
    Symbol,
    RTLD_LOCAL,
    RTLD_NOW,
};
#[cfg(windows)]
use libloading::os::windows::{
    Library as LLNativeLibrary, // LL means libloading
    Symbol,
};
use log::{debug, info, trace};
use std::fmt::format;
use std::fs;
use std::ops::Deref;
use std::path::PathBuf;
use std::str::FromStr;
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
        }
    }
}

/// A struct that stores OS-native DLLs.
/// The actual handling of DLLs is done using libloading.
///
/// By owning not only the library itself but also its dependencies,
/// it prevents the library and its dependent parts from being unloaded prematurely.
pub struct NativeLibrary {
    pub raw_library: LLNativeLibrary,
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
    pub(crate) fn new_native_library(
        raw_library: LLNativeLibrary,
        raw_dependencies: Vec<LLNativeLibrary>,
    ) -> Self {
        Library::NativeLibrary(NativeLibrary {
            raw_library,
            raw_dependencies,
        })
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
            Library::NativeLibrary(NativeLibrary {
                raw_library: lib, ..
            }) => {
                let symbol: Symbol<<(Args, Res) as IOToFn>::Output> =
                    unsafe { lib.get(name.as_bytes())? };
                Ok(Function::LLFunction(symbol))
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

fn is_wasi(platform: &str) -> bool {
    platform.contains("wasi")
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

#[cfg(unix)]
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
