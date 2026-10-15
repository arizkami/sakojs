//! Global package store: each `name@version` is downloaded, verified and
//! extracted once into `~/.cache/sako/npm/pkgs/`, then hard-linked into
//! every project's `node_modules` (falling back to copies across devices).

use crate::registry::{Manifest, agent};
use base64::Engine as _;
use std::io::Read;
use std::path::{Component, Path, PathBuf};

pub struct Store {
  root: PathBuf,
}

const READY: &str = ".sako-ok";

/// `$SAKO_NPM_CACHE`, else `~/.cache/sako/npm`.
pub fn cache_root() -> PathBuf {
  std::env::var_os("SAKO_NPM_CACHE").map(PathBuf::from).unwrap_or_else(|| {
    let base = std::env::var_os("XDG_CACHE_HOME")
      .map(PathBuf::from)
      .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
      .unwrap_or_else(std::env::temp_dir);
    base.join("sako").join("npm")
  })
}

impl Store {
  pub fn new() -> Store {
    Store { root: cache_root() }
  }

  pub fn package_dir(&self, m: &Manifest) -> PathBuf {
    self.root.join("pkgs").join(format!("{}@{}", m.name.replace('/', "+"), m.version))
  }

  pub fn has(&self, m: &Manifest) -> bool {
    self.package_dir(m).join(READY).exists()
  }

  /// Downloads, verifies and extracts a package unless already stored.
  pub fn ensure(&self, m: &Manifest) -> Result<PathBuf, String> {
    let dir = self.package_dir(m);
    if dir.join(READY).exists() {
      return Ok(dir);
    }
    let what = format!("{}@{}", m.name, m.version);
    if m.tarball.is_empty() {
      return Err(format!("{what}: registry has no tarball"));
    }
    let mut res = agent().get(&m.tarball).call().map_err(|e| format!("{what}: download failed: {e}"))?;
    let data = res
      .body_mut()
      .with_config()
      .limit(2 * 1024 * 1024 * 1024)
      .read_to_vec()
      .map_err(|e| format!("{what}: download failed: {e}"))?;
    verify(&data, &m.integrity).map_err(|e| format!("{what}: {e}"))?;

    let tmp = dir.with_extension(format!("tmp{}-{}", std::process::id(), rand_suffix()));
    let _ = std::fs::remove_dir_all(&tmp);
    extract(&data, &tmp).map_err(|e| format!("{what}: extract failed: {e}"))?;
    std::fs::write(tmp.join(READY), b"").map_err(|e| format!("{what}: {e}"))?;
    if let Some(parent) = dir.parent() {
      std::fs::create_dir_all(parent).map_err(|e| format!("{what}: {e}"))?;
    }
    if std::fs::rename(&tmp, &dir).is_err() {
      // Another install stored it first (or a stale partial dir is there).
      if dir.join(READY).exists() {
        let _ = std::fs::remove_dir_all(&tmp);
      } else {
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::rename(&tmp, &dir).map_err(|e| format!("{what}: {e}"))?;
      }
    }
    Ok(dir)
  }
}

impl Default for Store {
  fn default() -> Self {
    Self::new()
  }
}

fn rand_suffix() -> u64 {
  use std::hash::{BuildHasher, Hasher};
  let mut h = std::collections::hash_map::RandomState::new().build_hasher();
  h.write_u64(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64));
  h.finish()
}

/// Checks Subresource Integrity (`sha512-<base64>`, also `sha1-...`; with
/// several hashes, the strongest supported one).
pub fn verify(data: &[u8], integrity: &str) -> Result<(), String> {
  use sha2::Digest;
  let entries: Vec<&str> = integrity.split_whitespace().collect();
  if let Some(want) = entries.iter().find_map(|e| e.strip_prefix("sha512-")) {
    let got = base64::engine::general_purpose::STANDARD.encode(sha2::Sha512::digest(data));
    return if got == want { Ok(()) } else { Err("integrity check failed (sha512 mismatch)".into()) };
  }
  if let Some(want) = entries.iter().find_map(|e| e.strip_prefix("sha1-")) {
    let digest = sha1_smol::Sha1::from(data).digest().bytes();
    let ok = if want.len() == 40 && want.bytes().all(|b| b.is_ascii_hexdigit()) {
      want.eq_ignore_ascii_case(&sha1_smol::Sha1::from(data).digest().to_string())
    } else {
      base64::engine::general_purpose::STANDARD.encode(digest) == want
    };
    return if ok { Ok(()) } else { Err("integrity check failed (sha1 mismatch)".into()) };
  }
  if integrity.is_empty() {
    return Ok(()); // registry provided none
  }
  Err(format!("unsupported integrity: {integrity}"))
}

/// Extracts an npm tarball, dropping its top-level directory ("package/").
fn extract(tgz: &[u8], dest: &Path) -> std::io::Result<()> {
  std::fs::create_dir_all(dest)?;
  let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(tgz));
  for entry in archive.entries()? {
    let mut entry = entry?;
    let path = entry.path()?.into_owned();
    let mut rel = PathBuf::new();
    for c in path.components().skip(1) {
      match c {
        Component::Normal(p) => rel.push(p),
        Component::CurDir => {}
        // Never write outside the package directory.
        _ => {
          rel = PathBuf::new();
          break;
        }
      }
    }
    if rel.as_os_str().is_empty() {
      continue;
    }
    let target = dest.join(&rel);
    match entry.header().entry_type() {
      tar::EntryType::Directory => std::fs::create_dir_all(&target)?,
      tar::EntryType::Regular | tar::EntryType::Continuous => {
        if let Some(parent) = target.parent() {
          std::fs::create_dir_all(parent)?;
        }
        let mut data = Vec::with_capacity(entry.size() as usize);
        entry.read_to_end(&mut data)?;
        std::fs::write(&target, &data)?;
        #[cfg(unix)]
        {
          use std::os::unix::fs::PermissionsExt;
          let exec = entry.header().mode().map_or(false, |m| m & 0o111 != 0);
          std::fs::set_permissions(&target, std::fs::Permissions::from_mode(if exec { 0o755 } else { 0o644 }))?;
        }
      }
      _ => {} // links and special files are not used by npm packages
    }
  }
  Ok(())
}

/// Recreates `src` at `dst` with hard links (copies if linking fails).
pub fn link_tree(src: &Path, dst: &Path) -> std::io::Result<()> {
  std::fs::create_dir_all(dst)?;
  for entry in std::fs::read_dir(src)? {
    let entry = entry?;
    let name = entry.file_name();
    if name == READY {
      continue;
    }
    let from = entry.path();
    let to = dst.join(&name);
    if entry.file_type()?.is_dir() {
      link_tree(&from, &to)?;
    } else if std::fs::hard_link(&from, &to).is_err() {
      std::fs::copy(&from, &to)?;
    }
  }
  Ok(())
}

/// Creates `node_modules/.bin/<cmd>` symlinks for a package installed at
/// `pkg_dir` (whose parent is that `node_modules`).
pub fn link_bins(pkg_dir: &Path, install_name: &str, bins: &[(String, String)]) -> std::io::Result<()> {
  if bins.is_empty() {
    return Ok(());
  }
  // node_modules/<name> or node_modules/@scope/<name>
  let mut node_modules = pkg_dir.parent().unwrap_or(pkg_dir);
  if install_name.starts_with('@') {
    node_modules = node_modules.parent().unwrap_or(node_modules);
  }
  let bin_dir = node_modules.join(".bin");
  std::fs::create_dir_all(&bin_dir)?;
  for (cmd, file) in bins {
    let cmd = cmd.rsplit('/').next().unwrap_or(cmd);
    if cmd.is_empty() || cmd.starts_with('.') {
      continue;
    }
    let file = file.trim_start_matches("./");
    let target = pkg_dir.join(file);
    if !target.exists() {
      continue;
    }
    #[cfg(unix)]
    {
      use std::os::unix::fs::PermissionsExt;
      let _ = std::fs::set_permissions(&target, std::fs::Permissions::from_mode(0o755));
      let link = bin_dir.join(cmd);
      let _ = std::fs::remove_file(&link);
      std::os::unix::fs::symlink(Path::new("..").join(install_name).join(file), &link)?;
    }
    #[cfg(not(unix))]
    {
      let _ = (&bin_dir, cmd, &target);
    }
  }
  Ok(())
}

#[cfg(test)]
mod tests {
  use super::*;

  #[test]
  fn integrity() {
    // printf 'hello' | openssl dgst -sha512 -binary | base64
    let sri = "sha512-m3HSJL1i83hdltRq0+o9czGb+8KJDKra4t/3JRlnPKcjI8PZm6XBHXx6zG4UuMXaDEZjR1wuXDre9G9zvN7AQw==";
    assert!(verify(b"hello", sri).is_ok());
    assert!(verify(b"hellO", sri).is_err());
    assert!(verify(b"hello", "sha1-aaf4c61ddcc5e8a2dabede0f3b482cd9aea9434d").is_ok());
  }
}
