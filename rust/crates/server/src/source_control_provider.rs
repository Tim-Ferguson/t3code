//! The interface every source control host implements (port of `SourceControlProvider.ts`):
//! reading, listing and creating change requests (pull requests, merge requests), repository
//! lookup and creation, the default branch, and checking a change request out.
//!
//! Every method takes a working directory, because most hosts learn the repository from the
//! checkout's remotes, and an optional `ProviderContext`: the host the registry detected for
//! that checkout and the remote it came from. A bound provider (see `bind_context`) fills the
//! context in, which is how the registry hands out providers.
use futures_util::future::BoxFuture;
use std::sync::Arc;
use t3_contracts::{
    ChangeRequest, SourceControlProviderError, SourceControlProviderInfo, SourceControlProviderKind,
    SourceControlRepositoryCloneUrls, SourceControlRepositoryVisibility,
};

pub use crate::source_control_detect::{RefSelector, transport_safe_value};

pub type ProviderResult<T> = Result<T, SourceControlProviderError>;

/// The host a checkout's remote was detected as.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderContext {
    pub provider: SourceControlProviderInfo,
    pub remote_name: String,
    pub remote_url: String,
    /// An explicit web authority can disambiguate Forgejo logins sharing an SSH alias.
    pub requested_host: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListState {
    Open,
    Closed,
    Merged,
    All,
}

#[derive(Debug, Clone)]
pub struct ListChangeRequestsInput {
    pub cwd: String,
    pub context: Option<ProviderContext>,
    pub source: Option<RefSelector>,
    pub head_selector: String,
    pub state: ListState,
    pub limit: Option<u32>,
}

#[derive(Debug, Clone)]
pub struct GetChangeRequestInput {
    pub cwd: String,
    pub context: Option<ProviderContext>,
    pub reference: String,
}

#[derive(Debug, Clone)]
pub struct CreateChangeRequestInput {
    pub cwd: String,
    pub context: Option<ProviderContext>,
    pub source: Option<RefSelector>,
    pub target: Option<RefSelector>,
    pub base_ref_name: String,
    pub head_selector: String,
    pub title: String,
    pub body_file: String,
}

#[derive(Debug, Clone)]
pub struct RepositoryUrlsInput {
    pub cwd: String,
    pub context: Option<ProviderContext>,
    pub repository: String,
}

#[derive(Debug, Clone)]
pub struct CreateRepositoryInput {
    pub cwd: String,
    pub repository: String,
    pub visibility: SourceControlRepositoryVisibility,
}

#[derive(Debug, Clone)]
pub struct DefaultBranchInput {
    pub cwd: String,
    pub context: Option<ProviderContext>,
}

#[derive(Debug, Clone)]
pub struct CheckoutChangeRequestInput {
    pub cwd: String,
    pub context: Option<ProviderContext>,
    pub reference: String,
    pub force: bool,
}

/// What a chat message's link points at, for hosts that can read it without a checkout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkSubject {
    pub title: String,
    pub body: Option<String>,
}

pub trait SourceControlProvider: Send + Sync {
    fn kind(&self) -> SourceControlProviderKind;

    /// `None` synchronously for a URL this host does not resolve, without starting a lookup.
    fn resolve_link(
        &self,
        _cwd: &str,
        _url: &url::Url,
    ) -> Option<BoxFuture<'static, ProviderResult<LinkSubject>>> {
        None
    }

    fn list_change_requests(
        &self,
        input: ListChangeRequestsInput,
    ) -> BoxFuture<'_, ProviderResult<Vec<ChangeRequest>>>;
    fn get_change_request(
        &self,
        input: GetChangeRequestInput,
    ) -> BoxFuture<'_, ProviderResult<ChangeRequest>>;
    fn create_change_request(
        &self,
        input: CreateChangeRequestInput,
    ) -> BoxFuture<'_, ProviderResult<()>>;
    fn get_repository_clone_urls(
        &self,
        input: RepositoryUrlsInput,
    ) -> BoxFuture<'_, ProviderResult<SourceControlRepositoryCloneUrls>>;
    fn create_repository(
        &self,
        input: CreateRepositoryInput,
    ) -> BoxFuture<'_, ProviderResult<SourceControlRepositoryCloneUrls>>;
    fn get_default_branch(
        &self,
        input: DefaultBranchInput,
    ) -> BoxFuture<'_, ProviderResult<Option<String>>>;
    fn checkout_change_request(
        &self,
        input: CheckoutChangeRequestInput,
    ) -> BoxFuture<'_, ProviderResult<()>>;
}

/// The provider for a kind nothing is registered for: every operation fails with its name.
pub struct UnsupportedProvider(pub SourceControlProviderKind);

impl UnsupportedProvider {
    fn error(&self, operation: &str, cwd: &str) -> SourceControlProviderError {
        SourceControlProviderError::new(
            self.0,
            operation,
            cwd,
            format!("No {} source control provider is registered.", self.0),
        )
    }
}

impl SourceControlProvider for UnsupportedProvider {
    fn kind(&self) -> SourceControlProviderKind {
        self.0
    }
    fn list_change_requests(
        &self,
        input: ListChangeRequestsInput,
    ) -> BoxFuture<'_, ProviderResult<Vec<ChangeRequest>>> {
        Box::pin(async move { Err(self.error("listChangeRequests", &input.cwd)) })
    }
    fn get_change_request(
        &self,
        input: GetChangeRequestInput,
    ) -> BoxFuture<'_, ProviderResult<ChangeRequest>> {
        Box::pin(async move {
            Err(self
                .error("getChangeRequest", &input.cwd)
                .with_reference(&transport_safe_value(&input.reference)))
        })
    }
    fn create_change_request(
        &self,
        input: CreateChangeRequestInput,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move {
            Err(self
                .error("createChangeRequest", &input.cwd)
                .with_reference(&transport_safe_value(&input.head_selector)))
        })
    }
    fn get_repository_clone_urls(
        &self,
        input: RepositoryUrlsInput,
    ) -> BoxFuture<'_, ProviderResult<SourceControlRepositoryCloneUrls>> {
        Box::pin(async move {
            Err(self
                .error("getRepositoryCloneUrls", &input.cwd)
                .with_repository(&transport_safe_value(&input.repository)))
        })
    }
    fn create_repository(
        &self,
        input: CreateRepositoryInput,
    ) -> BoxFuture<'_, ProviderResult<SourceControlRepositoryCloneUrls>> {
        Box::pin(async move {
            Err(self
                .error("createRepository", &input.cwd)
                .with_repository(&transport_safe_value(&input.repository)))
        })
    }
    fn get_default_branch(
        &self,
        input: DefaultBranchInput,
    ) -> BoxFuture<'_, ProviderResult<Option<String>>> {
        Box::pin(async move { Err(self.error("getDefaultBranch", &input.cwd)) })
    }
    fn checkout_change_request(
        &self,
        input: CheckoutChangeRequestInput,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        Box::pin(async move {
            Err(self
                .error("checkoutChangeRequest", &input.cwd)
                .with_reference(&transport_safe_value(&input.reference)))
        })
    }
}

/// A provider with the checkout's detected context filled in wherever the caller left it out
/// (`bindProviderContext`). `createRepository` takes no context: it names the host itself.
pub struct BoundProvider {
    pub inner: Arc<dyn SourceControlProvider>,
    pub context: ProviderContext,
}

impl SourceControlProvider for BoundProvider {
    fn kind(&self) -> SourceControlProviderKind {
        self.inner.kind()
    }
    fn resolve_link(
        &self,
        cwd: &str,
        url: &url::Url,
    ) -> Option<BoxFuture<'static, ProviderResult<LinkSubject>>> {
        self.inner.resolve_link(cwd, url)
    }
    fn list_change_requests(
        &self,
        mut input: ListChangeRequestsInput,
    ) -> BoxFuture<'_, ProviderResult<Vec<ChangeRequest>>> {
        input.context.get_or_insert_with(|| self.context.clone());
        self.inner.list_change_requests(input)
    }
    fn get_change_request(
        &self,
        mut input: GetChangeRequestInput,
    ) -> BoxFuture<'_, ProviderResult<ChangeRequest>> {
        input.context.get_or_insert_with(|| self.context.clone());
        self.inner.get_change_request(input)
    }
    fn create_change_request(
        &self,
        mut input: CreateChangeRequestInput,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        input.context.get_or_insert_with(|| self.context.clone());
        self.inner.create_change_request(input)
    }
    fn get_repository_clone_urls(
        &self,
        mut input: RepositoryUrlsInput,
    ) -> BoxFuture<'_, ProviderResult<SourceControlRepositoryCloneUrls>> {
        input.context.get_or_insert_with(|| self.context.clone());
        self.inner.get_repository_clone_urls(input)
    }
    fn create_repository(
        &self,
        input: CreateRepositoryInput,
    ) -> BoxFuture<'_, ProviderResult<SourceControlRepositoryCloneUrls>> {
        self.inner.create_repository(input)
    }
    fn get_default_branch(
        &self,
        mut input: DefaultBranchInput,
    ) -> BoxFuture<'_, ProviderResult<Option<String>>> {
        input.context.get_or_insert_with(|| self.context.clone());
        self.inner.get_default_branch(input)
    }
    fn checkout_change_request(
        &self,
        mut input: CheckoutChangeRequestInput,
    ) -> BoxFuture<'_, ProviderResult<()>> {
        input.context.get_or_insert_with(|| self.context.clone());
        self.inner.checkout_change_request(input)
    }
}
