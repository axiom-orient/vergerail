//! Typed image-generation items emitted by the pinned Codex extension.

use std::path::{Path, PathBuf};

/// Provider-reported failure for an image-generation item.
#[derive(Clone, Debug, Eq, PartialEq)]
#[non_exhaustive]
pub enum ImageGenerationFailure {
    /// The account reached an image-generation usage limit.
    UsageLimitExceeded {
        /// Provider limit identifier.
        limit_id: String,
        /// Unix timestamp at which the limit resets, when reported.
        resets_at: Option<i64>,
    },
}

impl ImageGenerationFailure {
    fn retained_bytes(&self) -> usize {
        match self {
            Self::UsageLimitExceeded { limit_id, .. } => limit_id.len(),
        }
    }
}

/// One image-generation item from Codex app-server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ImageGeneration {
    id: String,
    status: String,
    revised_prompt: Option<String>,
    result_base64: String,
    transparent_background: Option<bool>,
    failure: Option<ImageGenerationFailure>,
    saved_path: Option<PathBuf>,
}

impl ImageGeneration {
    pub(crate) fn new(
        id: String,
        status: String,
        revised_prompt: Option<String>,
        result_base64: String,
        transparent_background: Option<bool>,
        failure: Option<ImageGenerationFailure>,
        saved_path: Option<PathBuf>,
    ) -> Self {
        Self {
            id,
            status,
            revised_prompt,
            result_base64,
            transparent_background,
            failure,
            saved_path,
        }
    }

    /// Returns the Codex item identifier.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the provider-defined lifecycle status.
    #[must_use]
    pub fn status(&self) -> &str {
        &self.status
    }

    /// Returns the revised prompt produced by the image model, when reported.
    #[must_use]
    pub fn revised_prompt(&self) -> Option<&str> {
        self.revised_prompt.as_deref()
    }

    /// Returns the base64-encoded generated image bytes.
    ///
    /// The value is empty while generation is in progress or when generation failed.
    #[must_use]
    pub fn result_base64(&self) -> &str {
        &self.result_base64
    }

    /// Returns whether a transparent background was requested or selected, when reported.
    #[must_use]
    pub const fn transparent_background(&self) -> Option<bool> {
        self.transparent_background
    }

    /// Returns the typed provider failure, when generation failed.
    #[must_use]
    pub const fn failure(&self) -> Option<&ImageGenerationFailure> {
        self.failure.as_ref()
    }

    /// Returns the sandboxed saved path, when Codex persisted the generated image.
    #[must_use]
    pub fn saved_path(&self) -> Option<&Path> {
        self.saved_path.as_deref()
    }

    pub(crate) fn retained_bytes(&self) -> Option<usize> {
        let mut bytes = self.id.len().checked_add(self.status.len())?;
        bytes = bytes.checked_add(self.result_base64.len())?;
        if let Some(prompt) = self.revised_prompt.as_deref() {
            bytes = bytes.checked_add(prompt.len())?;
        }
        if let Some(path) = self.saved_path.as_deref() {
            bytes = bytes.checked_add(path.as_os_str().len())?;
        }
        if let Some(failure) = self.failure.as_ref() {
            bytes = bytes.checked_add(failure.retained_bytes())?;
        }
        Some(bytes)
    }
}
