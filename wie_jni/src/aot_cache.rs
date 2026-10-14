//! On-disk cache for `wie-core-arm-native`'s JIT output.
//!
//! Without this, `NativeExecutor` still JIT-compiles (it doesn't need a cache to
//! work), but every app launch re-compiles every region from scratch. Caching the
//! compiled artifacts under `filesDir/aot_cache/<hex key>` makes the second and
//! later launches skip that work. One file per cache key, no extra dependency
//! (plain hex, no sqlite).

use std::{fs, path::PathBuf};

use wie_core_arm_native::NativeCache;
use wie_util::{Result, WieError};

pub struct FileCache {
    directory: PathBuf,
}

impl FileCache {
    pub fn new(directory: PathBuf) -> Self {
        Self { directory }
    }

    fn path(&self, key: &[u8; 32]) -> PathBuf {
        let mut name = String::with_capacity(64);
        for byte in key {
            name.push_str(&format!("{byte:02x}"));
        }
        self.directory.join(name)
    }
}

impl NativeCache for FileCache {
    fn load(&mut self, key: &[u8; 32]) -> Result<Option<Vec<u8>>> {
        match fs::read(self.path(key)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(WieError::FatalError(error.to_string())),
        }
    }

    fn store(&mut self, key: &[u8; 32], artifact: &[u8]) -> Result<()> {
        fs::create_dir_all(&self.directory).map_err(|error| WieError::FatalError(error.to_string()))?;
        // Write-then-rename so a crash mid-write never leaves a corrupt cache file
        // that `load` would hand back as valid.
        let final_path = self.path(key);
        let tmp_path = final_path.with_extension("tmp");
        fs::write(&tmp_path, artifact).map_err(|error| WieError::FatalError(error.to_string()))?;
        fs::rename(&tmp_path, &final_path).map_err(|error| WieError::FatalError(error.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrips_and_overwrites() {
        let dir = tempfile_dir();
        let mut cache = FileCache::new(dir.clone());
        assert!(cache.load(&[0; 32]).unwrap().is_none());

        for bytes in [b"first".as_slice(), b"second, longer".as_slice()] {
            cache.store(&[7; 32], bytes).unwrap();
            assert_eq!(cache.load(&[7; 32]).unwrap().unwrap(), bytes);
        }
        let _ = fs::remove_dir_all(dir);
    }

    fn tempfile_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("wie_jni_aot_test_{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }
}
