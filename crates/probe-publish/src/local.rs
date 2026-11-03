//! The local (on-disk) publisher.
//!
//! Minimal by design: the authoritative local pipeline (snapshots, meta
//! files, retention) already lives in `probe-engine`'s publication module.
//! This writer exists so `CompositePublisher` and the engine hook have ONE
//! artifact-shaped local destination with the same idempotence contract as
//! the Gist publisher: identical content is a no-op, a changed file is
//! replaced atomically (temp + fsync + rename), never truncated in place.

use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::types::{
    revision_of, sha256_hex, validate_gist_file, ArtifactRef, PublishError, PublishErrorCode,
    PublishOutcome,
};
use crate::Publisher;

/// Publishes artifacts as files under a caller-supplied root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalPublisher {
    root: PathBuf,
}

impl LocalPublisher {
    /// The export root the artifacts land in.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// The configured root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn artifact_path(&self, artifact: &ArtifactRef) -> PathBuf {
        self.root.join(&artifact.gist_file)
    }
}

impl Publisher for LocalPublisher {
    async fn publish(&self, artifact: &ArtifactRef, content: &str) -> PublishOutcome {
        if let Err(error) = validate_gist_file(&artifact.gist_file) {
            return PublishOutcome::from(error);
        }
        let sha = sha256_hex(content.as_bytes());
        // Gate 1, no I/O: the artifact row already holds this content.
        if artifact.previous_sha256.as_deref() == Some(sha.as_str()) {
            return PublishOutcome::NoChange;
        }
        let path = self.artifact_path(artifact);
        // Gate 2: the file on disk already holds exactly this content (the
        // row's baseline can be lost; the bytes do not lie).
        match tokio::fs::read(&path).await {
            Ok(existing) if sha256_hex(&existing) == sha => return PublishOutcome::NoChange,
            // Unreadable for ANY reason (absent, locked on Windows, denied)
            // just means "not provably current": fall through and write.
            _ => {}
        }
        if let Err(error) = write_atomic(&path, content.as_bytes()).await {
            // One behavior for every local failure: `io`, retryable. A full
            // disk may recover; a broken root path will fail again, which is
            // exactly what "retryable" means -- MAY succeed, not WILL.
            return PublishOutcome::Failed {
                code: PublishErrorCode::Io,
                retryable: true,
                message: format!("write {} failed: {error}", path.display()),
            };
        }
        PublishOutcome::Published {
            revision: Some(revision_of(&sha)),
            gist_failed: None,
        }
    }

    /// The root must exist (or be creatable); that is the whole local
    /// health model.
    async fn healthcheck(&self, _gist_id: &str) -> Result<(), PublishError> {
        tokio::fs::create_dir_all(&self.root)
            .await
            .map_err(|error| {
                PublishError::terminal(
                    PublishErrorCode::Io,
                    format!("export root {} unusable: {error}", self.root.display()),
                )
            })
    }
}

/// Write `bytes` to `path` so a crash never leaves a half-written artifact:
/// a uniquely named temp file in the SAME directory (same volume, so the
/// rename is atomic), fsync, rename over the target. The directory entry
/// itself is not fsynced (Windows has no directory fsync); a lost rename
/// after a power cut costs one re-publish, not a corrupt file.
async fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await?;
    }
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .ok_or_else(|| {
            std::io::Error::new(
                ErrorKind::InvalidInput,
                "artifact path has no file name component",
            )
        })?;
    let temp = path.with_file_name(format!(
        "{file_name}.tmp-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|t| t.as_nanos())
            .unwrap_or(0)
    ));
    let write = async {
        let mut file = tokio::fs::File::create(&temp).await?;
        tokio::io::AsyncWriteExt::write_all(&mut file, bytes).await?;
        file.sync_all().await
    };
    if let Err(error) = write.await {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(error);
    }
    if let Err(error) = tokio::fs::rename(&temp, path).await {
        let _ = tokio::fs::remove_file(&temp).await;
        return Err(error);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// First publish writes; an identical second publish is a no-op on
    /// content bytes (no timestamp drift, no rewrite).
    #[tokio::test]
    async fn identical_content_is_a_no_op() {
        let root = tempfile::tempdir().unwrap();
        let publisher = LocalPublisher::new(root.path());
        let artifact = ArtifactRef::new("aaaaaaaa", "air-a1.yaml");

        let first = publisher.publish(&artifact, "content v1").await;
        let PublishOutcome::Published {
            revision,
            gist_failed,
        } = first
        else {
            panic!("expected Published, got {first:?}");
        };
        assert!(gist_failed.is_none());
        assert_eq!(
            revision.as_deref(),
            Some(crate::revision_of(&sha256_hex(b"content v1")).as_str())
        );

        // Same content, no previous sha known: gate 2 compares the disk.
        let again = publisher.publish(&artifact, "content v1").await;
        assert_eq!(again, PublishOutcome::NoChange);
        // Same content, the row's baseline agrees too: gate 1 fires first.
        let row = ArtifactRef {
            previous_sha256: Some(sha256_hex(b"content v1")),
            ..artifact.clone()
        };
        assert_eq!(
            publisher.publish(&row, "content v1").await,
            PublishOutcome::NoChange
        );
    }

    /// Changed content replaces the file atomically and leaves no temp
    /// files behind.
    #[tokio::test]
    async fn changed_content_replaces_the_file_atomically() {
        let root = tempfile::tempdir().unwrap();
        let publisher = LocalPublisher::new(root.path());
        let artifact = ArtifactRef::new("aaaaaaaa", "air-a1.yaml");
        publisher.publish(&artifact, "content v1").await;
        publisher.publish(&artifact, "content v2").await;
        assert_eq!(
            std::fs::read_to_string(root.path().join("air-a1.yaml")).unwrap(),
            "content v2"
        );
        let left_overs: Vec<_> = std::fs::read_dir(root.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            left_overs,
            vec!["air-a1.yaml".to_string()],
            "no temp files left"
        );
    }

    /// A file name that would escape the root (or break on Windows) is a
    /// config failure before any I/O.
    #[tokio::test]
    async fn hostile_file_names_are_refused() {
        let root = tempfile::tempdir().unwrap();
        let publisher = LocalPublisher::new(root.path());
        for hostile in ["../escape.yaml", "a/b.yaml", "a\\b.yaml", "", "a:b.yaml"] {
            let outcome = publisher
                .publish(&ArtifactRef::new("aaaaaaaa", hostile), "x")
                .await;
            match outcome {
                PublishOutcome::Failed {
                    code, retryable, ..
                } => {
                    assert_eq!(code, PublishErrorCode::Config, "{hostile:?}");
                    assert!(!retryable, "{hostile:?}");
                }
                other => panic!("{hostile:?} must be refused, got {other:?}"),
            }
        }
    }

    /// The health model: a creatable root is healthy, an uncreatable one is
    /// not.
    #[tokio::test]
    async fn healthcheck_creates_the_root() {
        let root = tempfile::tempdir().unwrap();
        let nested = root.path().join("exports").join("deep");
        let publisher = LocalPublisher::new(&nested);
        publisher.healthcheck("").await.unwrap();
        assert!(nested.is_dir());

        // A path component that is a FILE cannot become a directory.
        let blocked = root.path().join("file");
        std::fs::write(&blocked, "x").unwrap();
        let error = LocalPublisher::new(blocked.join("sub"))
            .healthcheck("")
            .await
            .unwrap_err();
        assert_eq!(error.code, PublishErrorCode::Io);
    }
}
