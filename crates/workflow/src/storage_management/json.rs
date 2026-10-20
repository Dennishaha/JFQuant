//! Bounded JSON I/O with exactly the existing serde representation and fingerprints.
use super::*;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::io::{self, BufWriter, Write};

const BUFFER: usize = 64 * 1024;

struct Counter(u64);
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("storage_json_size_overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
struct Hasher(Sha256);
impl Write for Hasher {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.0.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
pub(super) fn signature(value: &impl Serialize) -> Result<String> {
    hash(value, false)
}
pub(super) fn pretty_signature(value: &impl Serialize) -> Result<String> {
    hash(value, true)
}
fn hash(value: &impl Serialize, pretty: bool) -> Result<String> {
    let mut writer = BufWriter::with_capacity(BUFFER, Hasher(Sha256::new()));
    if pretty {
        serde_json::to_writer_pretty(&mut writer, value)?;
    } else {
        serde_json::to_writer(&mut writer, value)?;
    }
    writer.flush()?;
    let hasher = writer.into_inner().map_err(|e| e.into_error())?.0;
    Ok(format!(
        "sha256:{:x}",
        analytics_core::hash::Hex(hasher.finalize())
    ))
}
pub(super) fn pretty_size(value: &impl Serialize) -> Result<u64> {
    let mut writer = BufWriter::with_capacity(BUFFER, Counter(0));
    serde_json::to_writer_pretty(&mut writer, value)?;
    writer.flush()?;
    Ok(writer.get_ref().0)
}
pub(super) fn atomic_write(path: &Path, value: &impl Serialize) -> Result<()> {
    let parent = path.parent().ok_or("storage_json_parent_missing")?;
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".storage-json-")
        .tempfile_in(parent)?;
    {
        let mut writer = BufWriter::with_capacity(BUFFER, temporary.as_file_mut());
        serde_json::to_writer_pretty(&mut writer, value)?;
        writer.flush()?;
    }
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|e| e.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn storage_streamed_json_preserves_existing_bytes_and_fingerprints() {
        let value = serde_json::json!({"文本": "quoted \\\" and newline\n🙂".repeat(BUFFER), "values": [null, true, 1.25, u64::MAX]});
        assert_eq!(
            signature(&value).unwrap(),
            crate::signature(&value).unwrap()
        );
        let pretty = serde_json::to_vec_pretty(&value).unwrap();
        assert_eq!(
            pretty_signature(&value).unwrap(),
            analytics_storage::evidence_store::digest(&pretty)
        );
        assert_eq!(pretty_size(&value).unwrap(), pretty.len() as u64);
        let temporary = tempfile::tempdir().unwrap();
        let path = temporary.path().join("plan.json");
        atomic_write(&path, &value).unwrap();
        assert_eq!(fs::read(&path).unwrap(), pretty);
        assert_eq!(read::<serde_json::Value>(&path).unwrap(), value);
        atomic_write(&path, &vec!["replacement"]).unwrap();
        assert_eq!(
            fs::read(&path).unwrap(),
            serde_json::to_vec_pretty(&vec!["replacement"]).unwrap()
        );
        assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 1);
        struct Failure;
        impl Serialize for Failure {
            fn serialize<S: serde::Serializer>(
                &self,
                _: S,
            ) -> std::result::Result<S::Ok, S::Error> {
                Err(serde::ser::Error::custom("injected serialization failure"))
            }
        }
        let before = fs::read(&path).unwrap();
        assert!(atomic_write(&path, &Failure).is_err());
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(fs::read_dir(temporary.path()).unwrap().count(), 1);
    }
}
