use std::path::{Path, PathBuf};

/// Retain formal collection where requested; isolate every other invocation.
pub fn output_directory(collection: Option<PathBuf>) -> PathBuf {
    if let Some(path) = collection {
        std::fs::create_dir_all(&path).unwrap();
        return path;
    }
    let parent = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/formal/release-traces");
    std::fs::create_dir_all(&parent).unwrap();
    tempfile::Builder::new()
        .prefix("invocation-")
        .tempdir_in(parent)
        .unwrap()
        .keep()
}
