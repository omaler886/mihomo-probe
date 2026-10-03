//! Sub-Store asset management: the vendored known-good backend bundle is the
//! offline fallback (same strategy as subs-check-pro's embedded
//! `assets.EmbeddedSubStoreBackendVer`), a newer copy under `<root>/substore/`
//! wins when present, and `update_backend`/`ensure_frontend` pull fresh
//! releases from GitHub (optionally through a gh-proxy prefix).

use std::cmp::Ordering;
use std::io::Read;
use std::path::Path;

/// The vendored backend bundle (sub-store-org/Sub-Store release asset
/// `sub-store.min.js`, the script-engine build that runs inside the JS host).
pub const EMBEDDED_BUNDLE: &str = include_str!("../assets/sub-store.min.js");
pub const EMBEDDED_BACKEND_VERSION: &str = include_str!("../assets/BACKEND_VERSION");

const GITHUB_API: &str = "https://api.github.com";

#[derive(Debug, Clone)]
pub struct AssetPaths {
    pub dir: std::path::PathBuf,
    pub bundle: std::path::PathBuf,
    pub version: std::path::PathBuf,
    pub backend_path_file: std::path::PathBuf,
    pub frontend: std::path::PathBuf,
}

pub fn ensure_local_assets(dir: &Path) -> std::io::Result<AssetPaths> {
    std::fs::create_dir_all(dir)?;
    let bundle = dir.join("sub-store.min.js");
    let version = dir.join("backend.version");
    if !bundle.exists() {
        std::fs::write(&bundle, EMBEDDED_BUNDLE)?;
        std::fs::write(&version, EMBEDDED_BACKEND_VERSION.trim())?;
    } else if !version.exists() {
        std::fs::write(&version, EMBEDDED_BACKEND_VERSION.trim())?;
    }
    Ok(AssetPaths {
        dir: dir.to_path_buf(),
        bundle,
        version,
        backend_path_file: dir.join("backend-path.txt"),
        frontend: dir.join("frontend"),
    })
}

/// Resolve the bundle source to run: the on-disk copy (kept fresh by
/// `update_backend`) if present, else the vendored fallback.
pub fn load_bundle(paths: &AssetPaths) -> (String, String) {
    match (
        std::fs::read_to_string(&paths.bundle),
        std::fs::read_to_string(&paths.version),
    ) {
        (Ok(src), Ok(ver)) => (src, ver.trim().to_string()),
        _ => (
            EMBEDDED_BUNDLE.to_string(),
            EMBEDDED_BACKEND_VERSION.trim().to_string(),
        ),
    }
}

/// The frontend dist.zip is NOT vendored (several MB); it is downloaded on
/// first start. Backend-only operation keeps working without it.
pub async fn ensure_frontend(frontend_dir: &Path, gh_proxy: Option<&str>) -> Result<(), String> {
    if frontend_dir.join("index.html").exists() {
        return Ok(());
    }
    let (tag, url) =
        latest_release("sub-store-org/Sub-Store-Front-End", "dist.zip", gh_proxy).await?;
    let bytes = download(&url).await?;
    // The dist.zip nests everything under a versioned folder; extract to a
    // staging dir and flatten single-root layouts.
    let staging = frontend_dir.with_extension("staging");
    let _ = std::fs::remove_dir_all(&staging);
    std::fs::create_dir_all(&staging).map_err(|e| e.to_string())?;
    extract_zip(&bytes, &staging).map_err(|e| format!("extract {tag} dist.zip: {e}"))?;
    let root = single_root(&staging);
    std::fs::create_dir_all(frontend_dir).map_err(|e| e.to_string())?;
    for entry in std::fs::read_dir(&root).map_err(|e| e.to_string())? {
        let entry = entry.map_err(|e| e.to_string())?;
        let target = frontend_dir.join(entry.file_name());
        if std::fs::rename(entry.path(), &target).is_err() {
            rename_copy(&entry.path(), &target)?;
        }
    }
    let _ = std::fs::remove_dir_all(&staging);
    if !frontend_dir.join("index.html").exists() {
        return Err(format!("{tag} dist.zip did not contain index.html"));
    }
    tracing::info!(
        "sub-store frontend {} installed to {}",
        tag,
        frontend_dir.display()
    );
    Ok(())
}

/// If the extraction produced exactly one directory and nothing else, that
/// directory is the real root.
fn single_root(dir: &Path) -> std::path::PathBuf {
    let entries: Vec<_> = std::fs::read_dir(dir)
        .map(|rd| rd.filter_map(Result::ok).collect())
        .unwrap_or_default();
    if entries.len() == 1 && entries[0].path().is_dir() {
        return entries[0].path();
    }
    dir.to_path_buf()
}

/// rename() fails across filesystems; fall back to copy+delete.
fn rename_copy(from: &Path, to: &Path) -> Result<(), String> {
    if from.is_dir() {
        std::fs::create_dir_all(to).map_err(|e| e.to_string())?;
        for entry in std::fs::read_dir(from).map_err(|e| e.to_string())? {
            let entry = entry.map_err(|e| e.to_string())?;
            rename_copy(&entry.path(), &to.join(entry.file_name()))?;
        }
    } else {
        std::fs::copy(from, to).map_err(|e| e.to_string())?;
        let _ = std::fs::remove_file(from);
    }
    Ok(())
}

/// Replace the on-disk backend bundle when a newer release exists.
/// Returns whether an update happened.
pub async fn update_backend(paths: &AssetPaths, gh_proxy: Option<&str>) -> Result<bool, String> {
    let local = std::fs::read_to_string(&paths.version)
        .map(|v| v.trim().to_string())
        .unwrap_or_else(|_| EMBEDDED_BACKEND_VERSION.trim().to_string());
    let (tag, url) =
        latest_release("sub-store-org/Sub-Store", "sub-store.min.js", gh_proxy).await?;
    if version_cmp(&tag, &local) != Ordering::Greater {
        return Ok(false);
    }
    let bytes = download(&url).await?;
    let tmp = paths.bundle.with_extension("js.tmp");
    std::fs::write(&tmp, &bytes).map_err(|e| format!("write bundle: {e}"))?;
    std::fs::rename(&tmp, &paths.bundle).map_err(|e| format!("rename bundle: {e}"))?;
    std::fs::write(&paths.version, tag.trim()).map_err(|e| format!("write version: {e}"))?;
    tracing::info!("sub-store backend updated {local} -> {}", tag.trim());
    Ok(true)
}

async fn latest_release(
    repo: &str,
    asset_name: &str,
    gh_proxy: Option<&str>,
) -> Result<(String, String), String> {
    let client = reqwest::Client::builder()
        .user_agent("mihomo-probe")
        .build()
        .map_err(|e| format!("client: {e}"))?;
    let api = format!("{GITHUB_API}/repos/{repo}/releases/latest");
    let release: serde_json::Value = client
        .get(&api)
        .send()
        .await
        .map_err(|e| format!("{api}: {e}"))?
        .json()
        .await
        .map_err(|e| format!("{api}: {e}"))?;
    let tag = release["tag_name"]
        .as_str()
        .ok_or_else(|| format!("{api}: missing tag_name"))?
        .to_string();
    let assets = release["assets"]
        .as_array()
        .ok_or_else(|| format!("{api}: missing assets"))?;
    let url = assets
        .iter()
        .find(|a| a["name"].as_str() == Some(asset_name))
        .and_then(|a| a["browser_download_url"].as_str())
        .ok_or_else(|| format!("{repo} release {tag}: no asset {asset_name}"))?
        .to_string();
    // gh-proxy prefixes the file download, never the API call (mirrors
    // subs-check-pro's WarpURL strategy).
    let download_url = match gh_proxy {
        Some(prefix) if !prefix.is_empty() => {
            format!("{}/{}", prefix.trim_end_matches('/'), url)
        }
        _ => url,
    };
    Ok((tag, download_url))
}

async fn download(url: &str) -> Result<Vec<u8>, String> {
    let bytes = reqwest::get(url)
        .await
        .map_err(|e| format!("{url}: {e}"))?
        .error_for_status()
        .map_err(|e| format!("{url}: {e}"))?
        .bytes()
        .await
        .map_err(|e| format!("{url}: {e}"))?;
    Ok(bytes.to_vec())
}

fn extract_zip(bytes: &[u8], dest: &Path) -> Result<(), String> {
    let mut archive =
        zip::ZipArchive::new(std::io::Cursor::new(bytes)).map_err(|e| e.to_string())?;
    for i in 0..archive.len() {
        let mut file = archive.by_index(i).map_err(|e| e.to_string())?;
        let Some(rel) = file.enclosed_name() else {
            continue; // zip-slip: skip entries escaping the archive root
        };
        let out = dest.join(rel);
        if file.is_dir() {
            std::fs::create_dir_all(&out).map_err(|e| e.to_string())?;
        } else {
            if let Some(parent) = out.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            let mut buf = Vec::new();
            file.read_to_end(&mut buf).map_err(|e| e.to_string())?;
            std::fs::write(&out, buf).map_err(|e| e.to_string())?;
        }
    }
    Ok(())
}

/// Numeric, component-wise semver-ish comparison; non-numeric components
/// compare lexically. Good enough for release tags like `2.42.2`.
fn version_cmp(a: &str, b: &str) -> Ordering {
    let parse = |v: &str| -> Vec<(i64, String)> {
        v.trim_start_matches('v')
            .split(['.', '-', '+'])
            .map(|c| match c.parse::<i64>() {
                Ok(n) => (n, String::new()),
                Err(_) => (i64::MAX, c.to_string()),
            })
            .collect()
    };
    let (va, vb) = (parse(a), parse(b));
    for i in 0..va.len().max(vb.len()) {
        let ca = va.get(i).cloned().unwrap_or((0, String::new()));
        let cb = vb.get(i).cloned().unwrap_or((0, String::new()));
        match ca.cmp(&cb) {
            Ordering::Equal => continue,
            other => return other,
        }
    }
    Ordering::Equal
}

/// Load (or generate once) the secret backend URL prefix, `/`-prefixed.
pub fn load_or_create_backend_path(paths: &AssetPaths) -> std::io::Result<String> {
    if let Ok(existing) = std::fs::read_to_string(&paths.backend_path_file) {
        let existing = existing.trim();
        if !existing.is_empty() {
            let mut p = existing.to_string();
            if !p.starts_with('/') {
                p.insert(0, '/');
            }
            return Ok(p);
        }
    }
    let mut bytes = [0u8; 12];
    getrandom::fill(&mut bytes)
        .map_err(|e| std::io::Error::other(format!("random backend path: {e}")))?;
    let hex: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&paths.backend_path_file, &hex)?;
    Ok(format!("/{hex}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_ordering() {
        assert_eq!(version_cmp("2.42.2", "2.42.2"), Ordering::Equal);
        assert_eq!(version_cmp("2.43.0", "2.42.2"), Ordering::Greater);
        assert_eq!(version_cmp("2.42", "2.42.2"), Ordering::Less);
        assert_eq!(version_cmp("v3.0.0-beta", "2.9.9"), Ordering::Greater);
    }

    #[test]
    fn embedded_bundle_is_nonempty_and_versioned() {
        assert!(EMBEDDED_BUNDLE.len() > 1_000_000);
        assert!(EMBEDDED_BACKEND_VERSION.trim().starts_with('2'));
        assert!(EMBEDDED_BUNDLE.contains("$httpClient"));
    }

    #[test]
    fn backend_path_generation_persists() {
        let dir =
            std::env::temp_dir().join(format!("probe-substore-assets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = ensure_local_assets(&dir).unwrap();
        let first = load_or_create_backend_path(&paths).unwrap();
        assert!(first.starts_with('/') && first.len() > 20);
        assert_eq!(load_or_create_backend_path(&paths).unwrap(), first);
        // Vendored bundle materialized on disk.
        assert!(paths.bundle.exists());
        assert!(std::fs::read_to_string(&paths.version)
            .unwrap()
            .starts_with('2'));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
