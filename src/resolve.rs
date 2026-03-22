use crate::dependency::Dependency;
use crate::dllpack_file::{DllPackFile, PlatformManifest};
use crate::download::{cached_download_lib, cached_download_manifest, DllInfo, ManifestInfo};
use anyhow::{anyhow, Result};
use log::debug;
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fmt::Display;
use std::path::PathBuf;
use url::Url;

#[derive(Debug)]
pub enum ResolveError {
    PlatformNotSupported(String),
}

impl Display for ResolveError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ResolveError::PlatformNotSupported(platform) => {
                write!(f, "Platform {} is not supported", platform)
            }
        }
    }
}

/// Implementation of the DFS process for `fetch_manifests`.
fn fetch_manifests_inner(
    base_info: &ManifestInfo,
    work_dir: &PathBuf,
    platform: &str,
    result_map: &mut BTreeMap<ManifestInfo, PlatformManifest>,
    dependency_map: &mut BTreeMap<ManifestInfo, Vec<ManifestInfo>>,
    reverse_dependency_map: &mut BTreeMap<ManifestInfo, Vec<ManifestInfo>>,
) -> Result<()> {
    cached_download_manifest(&base_info)?;

    let file = DllPackFile::from_file(&base_info.path)?;
    let manifest = file.manifest;

    let Some(p_manifest) = manifest.platforms.get(platform) else {
        return Err(anyhow!(ResolveError::PlatformNotSupported(
            platform.to_string()
        )));
    };

    result_map.insert(base_info.clone(), p_manifest.clone());

    let mut deps = Vec::new();

    for dep in &p_manifest.dependencies {
        match dep {
            Dependency::DllPack { url } => {
                let info = ManifestInfo::from_input(url, work_dir)?;
                deps.push(info.clone());

                if !result_map.contains_key(&info) {
                    fetch_manifests_inner(
                        &info,
                        work_dir,
                        platform,
                        result_map,
                        dependency_map,
                        reverse_dependency_map,
                    )?;
                }

                reverse_dependency_map
                    .entry(info.clone())
                    .or_insert_with(Vec::new)
                    .push(base_info.clone());
            }
            _ => {}
        }
    }

    dependency_map.insert(base_info.clone(), deps);

    Ok(())
}

/// Recursively downloads and processes manifests using DFS, building a map of dependencies
/// and reverse dependencies.
fn fetch_manifests(
    base_url: &Url,
    work_dir: &PathBuf,
    platform: &str,
) -> Result<(
    ManifestInfo,
    BTreeMap<ManifestInfo, PlatformManifest>,
    BTreeMap<ManifestInfo, Vec<ManifestInfo>>,
    BTreeMap<ManifestInfo, Vec<ManifestInfo>>,
)> {
    let mut result_map = BTreeMap::new();
    let mut dependency_map = BTreeMap::new();
    let mut reverse_dependency_map = BTreeMap::new();

    let base_info = ManifestInfo::from_input(base_url, work_dir)?;

    fetch_manifests_inner(
        &base_info,
        work_dir,
        platform,
        &mut result_map,
        &mut dependency_map,
        &mut reverse_dependency_map,
    )?;

    Ok((
        base_info,
        result_map,
        dependency_map,
        reverse_dependency_map,
    ))
}

/// Resolves dependencies, ensuring all necessary libraries are downloaded
/// and available in the correct order.
/// Return value is a tuple of the main library and a vector of dependencies.
pub fn resolve(
    base_url: &Url,
    work_dir: &PathBuf,
    platform: &str,
) -> Result<(DllInfo, Vec<DllInfo>)> {
    let (base_info, result_map, dependency_map, reverse_dependency_map) =
        fetch_manifests(base_url, work_dir, platform)?;

    let mut available = Vec::new();
    let mut remain_deps_counts =
        BTreeMap::from_iter(dependency_map.iter().map(|(k, v)| (k.clone(), v.len())));
    let mut unresolved_count = result_map.len();
    let mut dependency_load_order = Vec::new();

    for (m_info, count) in remain_deps_counts.iter() {
        if count == &0 {
            available.push(m_info.clone());
            unresolved_count -= 1;
            if &m_info.url != base_url {
                dependency_load_order.push(m_info.clone());
            }
        }
    }

    while !available.is_empty() {
        let url = available.pop().unwrap();

        for dep in reverse_dependency_map.get(&url).unwrap_or(&vec![]) {
            let count = remain_deps_counts.get_mut(dep).unwrap();
            *count -= 1;

            if *count == 0 {
                available.push(dep.clone());
                unresolved_count -= 1;

                if &dep.url != base_url {
                    dependency_load_order.push(dep.clone());
                }
            }
        }
    }

    if unresolved_count > 0 {
        return Err(anyhow!(
            "Failed to resolve all dependencies for {}. It may be a circular dependency.",
            platform
        ));
    }

    let mut dependency_load_order_paths = Vec::new();

    for m_info in dependency_load_order.iter() {
        let manifest = result_map.get(m_info).unwrap();

        let dll_info = DllInfo::from_input(
            &manifest.url,
            &manifest.name.as_ref().map(String::as_str),
            work_dir,
        )?;
        cached_download_lib(&dll_info)?;
        dependency_load_order_paths.push(dll_info);
    }

    let manifest = result_map.get(&base_info).unwrap();
    let dll_info = DllInfo::from_input(
        &manifest.url,
        &manifest.name.as_ref().map(String::as_str),
        work_dir,
    )?;
    cached_download_lib(&dll_info)?;

    Ok((dll_info, dependency_load_order_paths))
}

/// Gathers all locally cached dependencies (DllPacks and Dlls) for **all platforms**
/// of the given dllpack, without triggering any downloads.
/// If the main dllpack is not cached locally, it returns `Ok(None)`.
///
/// # Returns
/// * `Ok(Some(Vec<(String, PathBuf)>))` If a cache exists.
///    A Vec of tuple (url of data, cached path) is returned if the data to be erased exists in the cache.
/// * `Ok(None)` If the top-level dllpack manifest is not found in the local cache.
/// * `Err(...)` If some I/O or parsing error occurs.
pub fn get_all_cached_dependencies(
    dllpack_url: &Url,
    work_dir: &PathBuf,
) -> Result<Option<Vec<(String, PathBuf)>>> {
    // Build a ManifestInfo for the top-level URL
    let base_info = ManifestInfo::from_input(dllpack_url, work_dir)?;

    // If the main manifest file doesn't exist locally, we can't parse it -> return None
    if !base_info.path.exists() {
        return Ok(None);
    }

    // Parse the top-level dllpack file
    let base_file = DllPackFile::from_file(&base_info.path)
        .map_err(|e| anyhow!("Failed to parse the main dllpack file: {}", e))?;

    // Prepare a result structure
    let mut result = vec![(base_info.url.to_string(), base_info.path.clone())];

    debug!("aa {:?}", base_file);

    // We'll do a BFS (or DFS) to traverse all dependent dllpacks across all platforms,
    // but only for those that are already cached.
    let mut visited_manifests = BTreeSet::new();
    let mut queue = VecDeque::new();

    // Enqueue the top-level file
    visited_manifests.insert(base_info.clone());
    queue.push_back(base_file);

    while let Some(current_file) = queue.pop_front() {
        // For each platform in the current dllpack, gather dependencies
        for (_platform_name, p_manifest) in &current_file.manifest.platforms {
            let dll_info =
                DllInfo::from_input(&p_manifest.url, &p_manifest.name.as_deref(), work_dir)?;
            if let Some(p) = dll_info.exist_cache_dir() {
                result.push((dll_info.url.to_string(), p));
            }

            for dep in &p_manifest.dependencies {
                match dep {
                    // If the dependency is another dllpack, check if it's cached
                    Dependency::DllPack { url } => {
                        let sub_info = ManifestInfo::from_input(url, work_dir)?;

                        // If we haven't visited this sub-manifest yet and it's cached locally
                        if !visited_manifests.contains(&sub_info) && sub_info.path.exists() {
                            // Parse it
                            let sub_file = DllPackFile::from_file(&sub_info.path).map_err(|e| {
                                anyhow!("Failed to parse a dependent dllpack file: {}", e)
                            })?;
                            // Record it in the dependency list
                            result.push((url.to_string(), sub_info.path.clone()));
                            // Mark as visited and enqueue
                            visited_manifests.insert(sub_info);
                            queue.push_back(sub_file);
                        }
                    }
                    // If the dependency is a direct Dll
                    Dependency::RawLib { url, name } => {
                        let dll_info = DllInfo::from_input(url, &name.as_deref(), work_dir)?;
                        // If it's actually present, record it
                        if let Some(p) = dll_info.exist_cache_dir() {
                            result.push((dll_info.url.to_string(), p));
                        }
                    }
                }
            }
        }
    }

    // If we reach here, we did find and parse the main dllpack, so return Some(...)
    Ok(Some(result))
}

/// Downloads all manifests and libraries for the given dllpack URL and platform,
/// without loading them. Useful for pre-fetching plugins before use.
pub fn prefetch(url: &Url, work_dir: &PathBuf, platform: &str) -> Result<()> {
    resolve(url, work_dir, platform)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    #[test]
    fn test_resolve() {
        let work_dir = PathBuf::from_str("/home/nahco314/RustroverProjects/dll-pack/work").unwrap();
        let base_url = Url::from_str("http://0.0.0.0:8000/a.dllpack").unwrap();
        let platform = "wasm32-wasip1";

        let result = resolve(&base_url, &work_dir, platform).unwrap();

        println!("{:?}", result);
    }
}
