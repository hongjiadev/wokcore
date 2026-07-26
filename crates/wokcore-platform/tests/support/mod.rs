use tempfile::{TempDir, tempdir};

pub fn private_tempdir() -> TempDir {
    let directory = tempdir().expect("private test temporary directory");
    #[cfg(unix)]
    {
        use std::{fs, os::unix::fs::PermissionsExt};

        fs::set_permissions(directory.path(), fs::Permissions::from_mode(0o700))
            .expect("set private test temporary directory permissions");
    }
    directory
}
