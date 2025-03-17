mod file_location;
mod index_file;

use crate::{manifest::PackageManifestFile, source, source::ipfs::Cid};
use anyhow::bail;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const REG_DIR_NAME: &str = "registry";
const REG_CACHE_DIR_NAME: &str = "cache";
const DEFAULT_NAMESPACE_NAME: &str = "default";

/// A package from the official registry.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
pub struct Source {
    /// The base version specified for the package.
    pub version: semver::Version,
    /// The namespace this package resides in, if no there is no namespace in
    /// registry setup, this will be `None`.
    pub namespace: Option<String>,
}

/// A pinned instance of the registry source.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub struct Pinned {
    /// The registry package with base version.
    pub source: Source,
    /// The corresponding CID for this registry entry.
    pub cid: Cid,
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
    /// Name of the github organization holding the registry index repository.
    repo_org: String,
    /// Name of git repository holding the registry index.
    repo_name: String,
    /// Amount of characters used for defining each indentation level in the
    /// registry. Needed to match the chunk_size of the registry index's
    /// publihser (forc.pub) so that each dependency index file location can be
    /// calculated same in both sides.
    chunk_size: usize,
}

impl GithubRegistryResolver {
    /// Default github organization name that holds the registry git repo.
    const DEFAULT_GITHUB_ORG: &str = "kayagokalp";
    /// Default name of the repository that holds the registry git repo.
    const DEFAULT_REPO_NAME: &str = "forc.pub-index";

    pub fn new(repo_org: String, repo_name: String, chunk_size: usize) -> Self {
        Self {
            repo_org,
            repo_name,
            chunk_size,
        }
    }

    /// Returns a `GithubRegistryResolver` that automatically uses
    /// `Self::DEFAULT_GITHUB_ORG` and `Self::DEFAULT_REPO_NAME`.
    pub fn with_default_github(chunk_size: usize) -> Self {
        Self {
            repo_org: Self::DEFAULT_GITHUB_ORG.to_string(),
            repo_name: Self::DEFAULT_REPO_NAME.to_string(),
            chunk_size,
        }
    }
}

fn registry_dir() -> PathBuf {
    forc_util::user_forc_directory().join(REG_DIR_NAME)
}

fn cache_dir(namespace: Option<&str>) -> PathBuf {
    let base = registry_dir().join(REG_CACHE_DIR_NAME);
    match namespace {
        Some(ns) => base.join(ns),
        None => base,
    }
}

fn pkg_cache_dir(
    namespace: Option<&str>,
    pkg_name: &str,
    pkg_version: &semver::Version,
) -> PathBuf {
    cache_dir(namespace).join(format!("{pkg_name}+{pkg_version}"))
}

impl source::Pin for Source {
    type Pinned = Pinned;
    fn pin(&self, ctx: source::PinCtx) -> anyhow::Result<(Self::Pinned, PathBuf)> {
        let pkg_name = ctx.name;
        let pinned = Pinned {
            source: self.clone(),
            cid: todo!("calculate the cid"),
        };
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

fn with_tmp_fetch_index<F, O>(
    fetch_id: u64,
    pkg_name: &str,
    source: &Source,
    f: F,
) -> anyhow::Result<O>
where
    F: FnOnce() -> anyhow::Result<O>,
{
    todo!()
}
