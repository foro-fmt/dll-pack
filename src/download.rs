use anyhow::{anyhow, Result};
use log::{debug, trace};
use reqwest;
use std::fs;
use std::fs::DirBuilder;
use std::io::Write;
use std::path::PathBuf;
use url::Url;
use urlencoding;

/// Metadata about the source of the raw DLL (.so, .dll) and where it will be downloaded.
#[derive(Debug, Clone, Ord, PartialOrd, Eq, PartialEq)]
pub struct DllInfo {
    pub url: Url,
    pub name: String,
    pub path: PathBuf,
    pub cache_dir: Option<PathBuf>,
}

impl DllInfo {
    pub fn new(url: Url, name: String, path: PathBuf, cache_dir: Option<PathBuf>) -> Self {
        Self {
            url,
            name,
            path,
            cache_dir,
        }
    }

    pub fn from_input(url: &Url, name: &Option<&str>, dir_path: &PathBuf) -> Result<Self> {
        let e_url = urlencoding::encode(url.as_str());

        let last_of_url_path = url.path_segments().and_then(|s| s.last());
        let name = name
            .or(last_of_url_path)
            .ok_or(anyhow!("Could not get file name"))?;

        let cache_dir = dir_path.join(e_url.to_string());
        let path = cache_dir.join(name);

        Ok(Self::new(
            url.clone(),
            name.to_string(),
            path,
            Some(cache_dir),
        ))
    }

    pub fn wasm_module_cache_path(&self) -> PathBuf {
        self.path
            .parent()
            .unwrap()
            .join(format!("module-cache-{}.bin", self.name))
    }

    pub fn exist_cache_dir(&self) -> Option<PathBuf> {
        if let Some(cache_dir) = &self.cache_dir {
            if cache_dir.exists() {
                return Some(cache_dir.clone());
            }
        }

        None
    }
}

pub fn download_lib(dll_info: &DllInfo) -> Result<()> {
    debug!("downloading: {}", dll_info.path.display());

    let res = reqwest::blocking::get(dll_info.url.as_str())?;

    if !res.status().is_success() {
        return Err(anyhow!(
            "Failed to download {}: {}",
            dll_info.url,
            res.status()
        ));
    }

    let parent = dll_info.path.parent().unwrap();
    DirBuilder::new().recursive(true).create(parent)?;

    let tmp_path = parent.join(format!(".tmp.{}", dll_info.name));
    let mut file = fs::File::create(&tmp_path)?;

    let content = res.bytes()?;
    file.write_all(&content)?;
    drop(file);

    fs::rename(&tmp_path, &dll_info.path)?;

    Ok(())
}

pub fn cached_download_lib(dll_info: &DllInfo) -> Result<()> {
    if dll_info.path.exists() {
        trace!("cached: {}", dll_info.path.display());

        return Ok(());
    }

    download_lib(dll_info)
}

/// Metadata about the source of the manifest (.dllpack) and where it will be downloaded.
#[derive(Debug, Clone, Ord, PartialOrd, Eq, PartialEq)]
pub struct ManifestInfo {
    pub url: Url,
    pub path: PathBuf,
}

impl ManifestInfo {
    pub fn new(url: Url, path: PathBuf) -> Self {
        Self { url, path }
    }

    pub fn from_input(url: &Url, dir_path: &PathBuf) -> Result<Self> {
        let e_url = urlencoding::encode(url.as_str());
        let path = dir_path.join("_manifests").join(e_url.to_string());

        Ok(Self::new(url.clone(), path))
    }
}

pub fn download_manifest(manifest_info: &ManifestInfo) -> Result<()> {
    debug!("downloading: {}", manifest_info.path.display());

    let res = reqwest::blocking::get(manifest_info.url.as_str())?;

    if !res.status().is_success() {
        return Err(anyhow!(
            "Failed to download {}: {}",
            manifest_info.url,
            res.status()
        ));
    }

    let parent = manifest_info.path.parent().unwrap();
    DirBuilder::new().recursive(true).create(parent)?;

    let tmp_path = parent.join(format!(
        ".tmp.{}",
        manifest_info.path.file_name().unwrap().to_string_lossy()
    ));
    let mut file = fs::File::create(&tmp_path)?;

    let content = res.bytes()?;
    file.write_all(&content)?;
    drop(file);

    fs::rename(&tmp_path, &manifest_info.path)?;

    Ok(())
}

pub fn cached_download_manifest(manifest_info: &ManifestInfo) -> Result<()> {
    if manifest_info.path.exists() {
        trace!("cached: {}", manifest_info.path.display());

        return Ok(());
    }

    download_manifest(manifest_info)
}
