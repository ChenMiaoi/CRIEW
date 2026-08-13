//! Embedded vendored `b4` runtime extraction.
//!
//! `cargo install` only installs the final binary, so a repo-relative
//! `./vendor/b4/b4.sh` fallback disappears after installation. This module
//! keeps a minimal embedded copy of the vendored runtime and materializes it
//! under the CRIEW data directory on demand.

use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::infra::error::{CriewError, ErrorCode, Result};

#[derive(Debug, Clone, Copy)]
struct Asset {
    relative_path: &'static str,
    contents: &'static [u8],
    #[cfg_attr(not(unix), allow(dead_code))]
    executable: bool,
}

include!(concat!(env!("OUT_DIR"), "/b4_vendor_assets.rs"));

pub fn ensure_installed(data_dir: &Path) -> Result<Option<PathBuf>> {
    let root = installation_root(data_dir);
    // Even when this build was made without the optional vendor submodule,
    // validate the destination first.  A file at `vendor/` must not silently
    // turn an unavailable embedded fallback into a misleading `None` result.
    if let Some(parent) = root.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            CriewError::with_source(
                ErrorCode::B4,
                format!(
                    "failed to create embedded b4 directory {}",
                    parent.display()
                ),
                error,
            )
        })?;
    }
    if ASSETS.is_empty() {
        return Ok(None);
    }

    for asset in ASSETS {
        write_asset(&root, *asset)?;
    }

    Ok(Some(root.join("b4.sh")))
}

pub fn script_path(data_dir: &Path) -> PathBuf {
    installation_root(data_dir).join("b4.sh")
}

fn installation_root(data_dir: &Path) -> PathBuf {
    data_dir.join("vendor").join("b4")
}

fn write_asset(root: &Path, asset: Asset) -> Result<()> {
    let path = root.join(asset.relative_path);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|error| {
            CriewError::with_source(
                ErrorCode::B4,
                format!(
                    "failed to create embedded b4 directory {}",
                    parent.display()
                ),
                error,
            )
        })?;
    }

    let needs_write = match fs::read(&path) {
        Ok(existing) => existing != asset.contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(error) => {
            return Err(CriewError::with_source(
                ErrorCode::B4,
                format!("failed to read embedded b4 asset {}", path.display()),
                error,
            ));
        }
    };

    if needs_write {
        fs::write(&path, asset.contents).map_err(|error| {
            CriewError::with_source(
                ErrorCode::B4,
                format!("failed to write embedded b4 asset {}", path.display()),
                error,
            )
        })?;
    }

    #[cfg(unix)]
    if asset.executable {
        let permissions = fs::Permissions::from_mode(0o755);
        fs::set_permissions(&path, permissions).map_err(|error| {
            CriewError::with_source(
                ErrorCode::B4,
                format!(
                    "failed to mark embedded b4 asset executable {}",
                    path.display()
                ),
                error,
            )
        })?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    use super::{ensure_installed, script_path};

    #[test]
    fn ensure_installed_writes_runtime_vendor_tree() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        let temp_root = std::env::temp_dir().join(format!(
            "criew-b4-vendor-test-{}-{nonce}",
            std::process::id()
        ));

        let installed = ensure_installed(&temp_root).expect("embedded b4 install");
        if let Some(script) = installed {
            assert_eq!(script, script_path(&temp_root));
            assert!(script.exists(), "embedded b4 script should exist");
            assert!(
                temp_root.join("vendor/b4/src/b4/__init__.py").exists(),
                "embedded b4 Python runtime should exist"
            );
        } else {
            assert!(!script_path(&temp_root).exists());
        }

        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn ensure_installed_is_idempotent_for_existing_runtime_tree() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        let temp_root = std::env::temp_dir().join(format!(
            "criew-b4-vendor-repeat-test-{}-{nonce}",
            std::process::id()
        ));

        let first = ensure_installed(&temp_root).expect("first embedded b4 install");
        let second = ensure_installed(&temp_root).expect("second embedded b4 install");

        assert_eq!(first, second);
        if let Some(script) = second {
            assert_eq!(script, script_path(&temp_root));
            assert!(script.exists(), "embedded b4 script should still exist");
        }

        let _ = fs::remove_dir_all(&temp_root);
    }

    #[test]
    fn ensure_installed_reports_directory_conflicts() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system time before unix epoch")
            .as_nanos();
        let temp_root = std::env::temp_dir().join(format!(
            "criew-b4-vendor-conflict-test-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&temp_root).expect("create temp root");
        fs::write(temp_root.join("vendor"), "not a directory").expect("write blocking vendor file");

        let error = ensure_installed(&temp_root).expect_err("conflicting vendor path should fail");

        assert!(
            error
                .to_string()
                .contains("failed to create embedded b4 directory")
        );

        let _ = fs::remove_dir_all(&temp_root);
    }
}
