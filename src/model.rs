//! Models exposed by the authenticated Codex account.

/// One model returned by Codex app-server.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Model {
    pub(crate) id: String,
    pub(crate) model: String,
    pub(crate) display_name: String,
    pub(crate) description: String,
    pub(crate) hidden: bool,
    pub(crate) is_default: bool,
}

impl Model {
    /// Returns the stable catalog identifier supplied by app-server.
    #[must_use]
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Returns the model name accepted by session options.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// Returns the provider-supplied display name.
    #[must_use]
    pub fn display_name(&self) -> &str {
        &self.display_name
    }

    /// Returns the provider-supplied description.
    #[must_use]
    pub fn description(&self) -> &str {
        &self.description
    }

    /// Reports whether the provider marked this model as hidden.
    #[must_use]
    pub const fn is_hidden(&self) -> bool {
        self.hidden
    }

    /// Reports whether the provider marked this model as the default.
    #[must_use]
    pub const fn is_default(&self) -> bool {
        self.is_default
    }
}
