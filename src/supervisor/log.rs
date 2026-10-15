use std::fs::{File, OpenOptions};
use std::io;
use std::path::Path;

/// Opens two independent file descriptors pointing at the same append-mode
/// file (sharing the kernel-level offset), for use as a child's stdout and
/// stderr — mirroring the original pm2 config where `out_file` and
/// `error_file` were the same path.
pub fn open_log_files(path: &Path) -> io::Result<(File, File)> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    let file2 = file.try_clone()?;
    Ok((file, file2))
}
