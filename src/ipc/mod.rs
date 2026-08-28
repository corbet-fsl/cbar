mod client;
pub mod commands;
pub mod responses;
mod server;

use std::fs;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

use color_eyre::Result;
use color_eyre::eyre::{WrapErr, eyre};
use tracing::warn;

pub use commands::*;
pub use responses::Response;

#[derive(Debug)]
pub struct Ipc {
    path: PathBuf,
}

impl Ipc {
    /// Creates a new IPC instance.
    /// This can be used as both a server and client.
    pub fn new() -> Result<Self> {
        let runtime_dir = std::env::var_os("XDG_RUNTIME_DIR")
            .filter(|path| !path.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| eyre!("XDG_RUNTIME_DIR is required for secure IPC"))?;

        validate_runtime_dir(&runtime_dir, rustix::process::geteuid().as_raw())?;

        let ipc_socket_file = runtime_dir.join("ironbar-ipc.sock");

        if format!("{}", ipc_socket_file.display()).len() > 100 {
            warn!(
                "The IPC socket file's absolute path exceeds 100 bytes, the socket may fail to create."
            );
        }

        Ok(Self {
            path: ipc_socket_file,
        })
    }

    pub fn path(&self) -> &Path {
        self.path.as_path()
    }
}

fn validate_runtime_dir(runtime_dir: &Path, expected_uid: u32) -> Result<()> {
    let metadata = fs::symlink_metadata(runtime_dir)
        .wrap_err_with(|| format!("Unable to inspect {}", runtime_dir.display()))?;
    if !metadata.file_type().is_dir() {
        return Err(eyre!(
            "XDG_RUNTIME_DIR is not a real directory: {}",
            runtime_dir.display()
        ));
    }

    if metadata.uid() != expected_uid {
        return Err(eyre!(
            "XDG_RUNTIME_DIR {} is owned by uid {}, expected effective uid {expected_uid}",
            runtime_dir.display(),
            metadata.uid()
        ));
    }

    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(eyre!(
            "XDG_RUNTIME_DIR {} has mode {mode:03o}; group and other permissions must be zero",
            runtime_dir.display()
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::symlink;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn test_path(name: &str) -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        std::env::temp_dir().join(format!(
            "ironbar-runtime-{name}-{}-{}",
            std::process::id(),
            NEXT_ID.fetch_add(1, Ordering::Relaxed)
        ))
    }

    fn private_runtime_dir(name: &str) -> (PathBuf, u32) {
        let path = test_path(name);
        fs::create_dir(&path).expect("private runtime directory to be created");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
            .expect("private runtime directory mode to be set");
        let uid = fs::symlink_metadata(&path)
            .expect("private runtime directory metadata")
            .uid();
        (path, uid)
    }

    #[test]
    fn accepts_private_runtime_directory_owned_by_effective_user() {
        let (path, uid) = private_runtime_dir("private");
        validate_runtime_dir(&path, uid).expect("private owned runtime directory to be valid");
        fs::remove_dir(path).expect("test runtime directory cleanup");
    }

    #[test]
    fn rejects_runtime_directory_with_group_or_other_access() {
        let (path, uid) = private_runtime_dir("shared");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o750))
            .expect("shared runtime directory mode to be set");
        let error =
            validate_runtime_dir(&path, uid).expect_err("shared runtime directory rejected");
        assert!(error.to_string().contains("permissions must be zero"));
        fs::remove_dir(path).expect("test runtime directory cleanup");
    }

    #[test]
    fn rejects_runtime_directory_owned_by_another_uid() {
        let (path, uid) = private_runtime_dir("foreign");
        let foreign_uid = if uid == u32::MAX { uid - 1 } else { uid + 1 };
        let error = validate_runtime_dir(&path, foreign_uid)
            .expect_err("foreign runtime directory rejected");
        assert!(error.to_string().contains("expected effective uid"));
        fs::remove_dir(path).expect("test runtime directory cleanup");
    }

    #[test]
    fn rejects_runtime_directory_symlink() {
        let (target, uid) = private_runtime_dir("target");
        let link = test_path("symlink");
        symlink(&target, &link).expect("runtime directory symlink to be created");
        let error = validate_runtime_dir(&link, uid).expect_err("runtime symlink rejected");
        assert!(error.to_string().contains("not a real directory"));
        fs::remove_file(link).expect("test runtime symlink cleanup");
        fs::remove_dir(target).expect("test runtime target cleanup");
    }
}
