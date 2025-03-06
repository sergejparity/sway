use crate::{manifest::PackageManifestFile, source};
use anyhow::bail;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A package from the official registry.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
pub struct Source {
    /// The base version specified for the package.
    pub version: semver::Version,
}

/// A pinned instance of the registry source.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub struct Pinned {
    /// The registry package with base version.
    pub source: Source,
    /// The pinned version.
    pub version: semver::Version,
}

/// Possible namespace types forc can handle for the registry index. Which has
/// a direct effect on the calculated paths for package index locations. So for
/// correct operation it is curicial that the resolver and publisher (forc.pub)
/// is using the same namespace type.
pub enum NamespaceType {
    /// All packages laid out in the same layout in index. Meaning there are no
    /// extra layer of indirection for custom domains.
    Flat,
    /// Publishers can have their own domain own multiple packages. Which means
    /// there will be extra layer of indirection in the registry.
    WithDomain,
}

/// A resolver for registry index hosted as a github repo.
///
/// Given a package name and a version, a `GithubRegistryResolver` will be able
/// to resolve, fetch, pin a package through using the index hosted on a github
/// repository.
pub struct GithubRegistryResolver {
    /// Namespace type of the registry index used by this resolver.
    namespace: NamespaceType,
    /// Name of the github organization holding the registry index repository.
    repo_org: String,
    /// Name of github repository holding the registry index.
    repo_name: String,
    /// Amount of characters used for defining each indentation level in the
    /// registry. Needed to match the chunk_size of the registry index's
    /// publihser (forc.pub) so that each dependency index file location can be
    /// calculated same in both sides.
    chunk_size: usize,
}

impl source::Pin for Source {
    type Pinned = Pinned;
    fn pin(&self, _ctx: source::PinCtx) -> anyhow::Result<(Self::Pinned, PathBuf)> {
        bail!("registry dependencies are not yet supported");
    }
}

impl source::Fetch for Pinned {
    fn fetch(&self, _ctx: source::PinCtx, _local: &Path) -> anyhow::Result<PackageManifestFile> {
        bail!("registry dependencies are not yet supported");
    }
}

impl source::DepPath for Pinned {
    fn dep_path(&self, _name: &str) -> anyhow::Result<source::DependencyPath> {
        bail!("registry dependencies are not yet supported");
    }
}

impl From<Pinned> for source::Pinned {
    fn from(p: Pinned) -> Self {
        Self::Registry(p)
    }
}
