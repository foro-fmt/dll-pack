//! dll-pack is a library for packaging, distributing, and loading dynamic libraries (DLLs/shared objects)
//! with their dependencies. It provides functionality for:
//!
//! - Loading dynamic libraries from .dllpack files
//! - Managing library dependencies
//! - Cross-platform support including WASM
//! - Safe function calling interfaces

use crate::type_utils::{Caller, IOToFn};
#[cfg(windows)]
use libloading::os::windows::{Library as LLLibrary, Symbol};
use std::ops::Deref;
use std::path::PathBuf;
use url::Url;
use wasmtime::IntoFunc;
pub mod dependency;
pub mod dllpack_file;
pub mod download;
mod fs_utils;
pub mod load;
pub mod resolve;
pub mod target_triple;
mod type_utils;

// Re-export commonly used types and functions for convenience
pub use load::{load, load_with_platform, load_with_wasm, Function, Library};
pub use resolve::{download, ResolveError};
pub use target_triple::THIS_PLATFORM;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::load::{load_with_platform, Library};
    use anyhow::Result;
    use std::str::FromStr;

    #[test]
    fn it_works() {
        let mut result = load_with_platform(
            &Url::from_str("http://0.0.0.0:8000/a.dllpack").unwrap(),
            &PathBuf::from_str("/home/nahco314/RustroverProjects/dll-pack/work").unwrap(),
            "x86_64-unknown-linux",
        )
        .unwrap();

        let a = result.get_function::<(i32, i32), (i32)>("adding").unwrap();
        let res = a.call(&mut result, (2, 3));

        println!("{}", res);
    }

    #[test]
    fn two() {
        let start_time = std::time::Instant::now();
        run_single_cached_with_platform(
            &Url::from_str("https://github.com/nahco314/dll-pack-sample-adder/releases/download/v0.1.0/dll-pack-sample-adder.dllpack").unwrap(),
            &PathBuf::from_str("/home/nahco314/RustroverProjects/dll-pack/work").unwrap(),
            "x86_64-unknown-linux-gnu",
        |lib: &mut Library| -> Result<()> {
                let a = lib.get_function::<(i32, i32), (i32)>("adding_and_one")?;
                let res = a.call(lib, (2, 3));

                println!("{}", res);

                Ok(())
            },
        )
            .unwrap();

        println!("Elapsed: {:?}", start_time.elapsed());

        run_single_cached_with_platform(
            &Url::from_str("https://github.com/nahco314/dll-pack-sample-adder/releases/download/v0.1.0/dll-pack-sample-adder.dllpack").unwrap(),
            &PathBuf::from_str("/home/nahco314/RustroverProjects/dll-pack/work").unwrap(),
            "x86_64-unknown-linux-gnu",
            |lib: &mut Library| -> Result<()> {
                let a = lib.get_function::<(i32, i32), (i32)>("adding_and_one")?;
                let res = a.call(lib, (2, 3));

                println!("{}", res);

                Ok(())
            },
        )
            .unwrap();

        println!("Elapsed: {:?}", start_time.elapsed());
    }
}
