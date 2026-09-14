use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt},
    path::{Path, PathBuf},
};

use anyhow::{Context, ensure};
use serde::{Serialize, de::DeserializeOwned};

pub struct Storage {
    directory: PathBuf,
    _lock: File,
}

impl Storage {
    /// A damaged site's certificate must not prevent other sites from starting.
    pub fn certificate(&self, domain: &str) -> Option<crate::certificates::Certificate> {
        let result = self
            .read::<crate::certificates::CertificatePem>(&format!("{domain}.json"))
            .and_then(|pem| {
                pem.map(|pem| crate::certificates::Certificate::parse(domain, &pem))
                    .transpose()
            });
        match result {
            Ok(cert) => cert,
            Err(error) => {
                tracing::error!(%domain, %error, "cannot load saved certificate; will request a replacement");
                None
            }
        }
    }

    pub fn open(root: &Path, directory_url: &str) -> anyhow::Result<Self> {
        fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(root)?;
        let metadata = fs::symlink_metadata(root)?;
        ensure!(
            metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
            "data directory {} must be a real directory with permissions 0700",
            root.display()
        );
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(root.join("proxy.lock"))?;
        fs2::FileExt::try_lock_exclusive(&lock)
            .context("another proxy is using this data directory")?;
        // Separate accounts and certificates for each CA, especially staging vs production.
        let hash = openssl::sha::sha256(directory_url.as_bytes());
        let name: String = hash.iter().map(|b| format!("{b:02x}")).collect();
        let directory = root.join(name);
        fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .or_else(|e| {
                if e.kind() == std::io::ErrorKind::AlreadyExists {
                    Ok(())
                } else {
                    Err(e)
                }
            })?;
        let metadata = fs::symlink_metadata(&directory)?;
        ensure!(
            metadata.is_dir() && metadata.permissions().mode() & 0o077 == 0,
            "CA storage directory must have permissions 0700"
        );
        Ok(Self {
            directory,
            _lock: lock,
        })
    }

    pub fn read<T: DeserializeOwned>(&self, name: &str) -> anyhow::Result<Option<T>> {
        match fs::read(self.directory.join(name)) {
            Ok(bytes) => Ok(Some(
                serde_json::from_slice(&bytes).with_context(|| format!("invalid stored {name}"))?,
            )),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e).with_context(|| format!("cannot read stored {name}")),
        }
    }

    pub fn write<T: Serialize>(&self, name: &str, value: &T) -> anyhow::Result<()> {
        let mut temporary = tempfile::NamedTempFile::new_in(&self.directory)?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        serde_json::to_writer(&mut temporary, value)?;
        temporary.flush()?;
        temporary.as_file().sync_all()?;
        temporary.persist(self.directory.join(name))?;
        File::open(&self.directory)?.sync_all()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn corrupt_certificates_are_isolated_from_healthy_sites() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let storage = Storage::open(&root.path().join("state"), "https://ca.test")?;
        storage.write(
            "good.example.com.json",
            &crate::certificates::tests::fixture("good.example.com", 1000, 10000)?,
        )?;
        fs::write(storage.directory.join("bad.example.com.json"), "{truncated")?;
        assert!(storage.certificate("bad.example.com").is_none());
        storage.write(
            "bad.example.com.json",
            &crate::certificates::CertificatePem {
                chain: "invalid".into(),
                private_key: "invalid".into(),
            },
        )?;
        assert!(storage.certificate("bad.example.com").is_none());
        assert!(storage.certificate("good.example.com").is_some());
        Ok(())
    }

    #[test]
    fn storage_is_private_exclusive_and_survives_restart() -> anyhow::Result<()> {
        let root = tempfile::tempdir()?;
        let path = root.path().join("state");
        let store = Storage::open(&path, "https://ca.test/directory")?;
        store.write("value.json", &vec![1, 2])?;
        assert!(Storage::open(&path, "https://ca.test/directory").is_err());
        assert_eq!(
            fs::metadata(store.directory.join("value.json"))?
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        store.write("value.json", &vec![3])?;
        drop(store);
        let store = Storage::open(&path, "https://ca.test/directory")?;
        assert_eq!(store.read::<Vec<u8>>("value.json")?, Some(vec![3]));
        drop(store);
        let staging = Storage::open(&path, "https://staging.test/directory")?;
        assert_eq!(staging.read::<Vec<u8>>("value.json")?, None);
        Ok(())
    }
}
