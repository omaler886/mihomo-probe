//! The composite publisher: local first, Gist second.
//!
//! The local export is the floor (F-037's "keep the previous stable file"
//! rule): if the local write fails, the publish fails and the Gist is not
//! touched. A Gist failure after a local success is *reported*, not fatal --
//! it surfaces as `Published { gist_failed: Some(reason) }` and the caller
//! records it against the run. `NoChange` requires BOTH legs to agree.

use crate::gist::GistPublisher;
use crate::local::LocalPublisher;
use crate::types::{ArtifactRef, PublishError, PublishOutcome};
use crate::Publisher;

/// Publishes to the local root, then to the Gist.
#[derive(Debug, Clone)]
pub struct CompositePublisher {
    local: LocalPublisher,
    gist: GistPublisher,
}

impl CompositePublisher {
    /// The local leg goes first and gates the Gist leg.
    pub fn new(local: LocalPublisher, gist: GistPublisher) -> Self {
        Self { local, gist }
    }

    /// The local leg, for callers that report both destinations.
    pub fn local(&self) -> &LocalPublisher {
        &self.local
    }

    /// The Gist leg.
    pub fn gist(&self) -> &GistPublisher {
        &self.gist
    }
}

impl Publisher for CompositePublisher {
    async fn publish(&self, artifact: &ArtifactRef, content: &str) -> PublishOutcome {
        let local_outcome = self.local.publish(artifact, content).await;
        let local_revision = match &local_outcome {
            PublishOutcome::Published { revision, .. } => revision.clone(),
            _ => None,
        };
        let local_outcome = match local_outcome {
            // The floor: no local artifact, no Gist write (a Gist-only
            // update would diverge the two destinations).
            PublishOutcome::Failed {
                code,
                retryable,
                message,
            } => {
                return PublishOutcome::Failed {
                    code,
                    retryable,
                    message,
                };
            }
            outcome => outcome,
        };

        match self.gist.publish(artifact, content).await {
            // Gist agrees there is nothing to do; the overall answer follows
            // the local leg.
            PublishOutcome::NoChange => match local_outcome {
                PublishOutcome::NoChange => PublishOutcome::NoChange,
                PublishOutcome::Published { revision, .. } => PublishOutcome::Published {
                    revision,
                    gist_failed: None,
                },
                PublishOutcome::Failed {
                    code,
                    retryable,
                    message,
                } => PublishOutcome::Failed {
                    code,
                    retryable,
                    message,
                },
            },
            PublishOutcome::Published { revision, .. } => PublishOutcome::Published {
                revision: revision.or_else(|| local_revision.clone()),
                gist_failed: None,
            },
            // The Gist leg failed but the local artifact is in place: the
            // publish SUCCEEDS with the failure attached (documented choice;
            // the caller marks the run's publish status from `gist_failed`).
            PublishOutcome::Failed {
                code,
                retryable: _,
                message,
            } => PublishOutcome::Published {
                revision: local_revision,
                gist_failed: Some(format!("{}: {message}", code.as_str())),
            },
        }
    }

    /// Both legs must be healthy; the message names which one answered.
    async fn healthcheck(&self, gist_id: &str) -> Result<(), PublishError> {
        self.local
            .healthcheck(gist_id)
            .await
            .map_err(|error| PublishError {
                code: error.code,
                retryable: error.retryable,
                message: format!("local: {}", error.message),
                status: error.status,
                retry_after: error.retry_after,
            })?;
        self.gist
            .healthcheck(gist_id)
            .await
            .map_err(|error| PublishError {
                code: error.code,
                retryable: error.retryable,
                message: format!("gist: {}", error.message),
                status: error.status,
                retry_after: error.retry_after,
            })
    }
}
