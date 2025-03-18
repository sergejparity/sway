mod file_location;
pub mod index_file;

use crate::{manifest::PackageManifestFile, source, source::ipfs::Cid};
use anyhow::{anyhow, bail};
use file_location::{location_from_root, Namespace};
use index_file::IndexFile;
use serde::{Deserialize, Serialize};
use std::{
    path::{Path, PathBuf},
    str::FromStr,
};

const REG_DIR_NAME: &str = "registry";
const REG_CACHE_DIR_NAME: &str = "cache";

/// A package from the official registry.
#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd, Deserialize, Serialize)]
pub struct Source {
    /// The base version specified for the package.
    pub version: semver::Version,
    /// The namespace this package resides in, if no there is no namespace in
    /// registry setup, this will be `None`.
    pub namespace: Namespace,
}

/// A pinned instance of the registry source.
#[derive(Clone, Debug, Eq, Hash, PartialEq, Deserialize, Serialize)]
pub struct Pinned {
    /// The registry package with base version.
    pub source: Source,
    /// The corresponding CID for this registry entry.
    pub cid: Cid,
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
    /// The number of letters used to chunk package name.
    ///
    /// Example:
    /// If set to 2, and package name is "foobar", the index file location
    /// will be ".../fo/ob/ar/foobar".
    pub chunk_size: usize,
    /// Type of the namespacing is needed to determine whether to add domain at
    /// the beginnig of the file location.
    pub namespace: Namespace,
}

impl GithubRegistryResolver {
    /// Default github organization name that holds the registry git repo.
    const DEFAULT_GITHUB_ORG: &str = "kayagokalp";
    /// Default name of the repository that holds the registry git repo.
    const DEFAULT_REPO_NAME: &str = "dummy-forc.pub-index";
    /// Default chunking size of the repository that holds registry git repo.
    pub const DEFAULT_CHUNKING_SIZE: usize = 2;

    pub fn new(
        repo_org: String,
        repo_name: String,
        chunk_size: usize,
        namespace: Namespace,
    ) -> Self {
        Self {
            repo_org,
            repo_name,
            chunk_size,
            namespace,
        }
    }

    /// Returns a `GithubRegistryResolver` that automatically uses
    /// `Self::DEFAULT_GITHUB_ORG` and `Self::DEFAULT_REPO_NAME`.
    pub fn with_default_github(namespace: Namespace) -> Self {
        Self {
            repo_org: Self::DEFAULT_GITHUB_ORG.to_string(),
            repo_name: Self::DEFAULT_REPO_NAME.to_string(),
            chunk_size: Self::DEFAULT_CHUNKING_SIZE,
            namespace,
        }
    }
}

fn registry_dir() -> PathBuf {
    forc_util::user_forc_directory().join(REG_DIR_NAME)
}

fn cache_dir(namespace: &Namespace) -> PathBuf {
    let base = registry_dir().join(REG_CACHE_DIR_NAME);
    match namespace {
        Namespace::Flat => base,
        Namespace::Domain(ns) => base.join(ns),
    }
}

fn pkg_cache_dir(namespace: &Namespace, pkg_name: &str, pkg_version: &semver::Version) -> PathBuf {
    cache_dir(namespace).join(format!("{pkg_name}+{pkg_version}"))
}

/// The name to use for a package's identifier entry under the user's forc directory.
fn registry_package_dir_name(name: &str, pkg_version: &semver::Version) -> String {
    use std::hash::{Hash, Hasher};
    fn hash_version(pkg_version: &semver::Version) -> u64 {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        pkg_version.hash(&mut hasher);
        hasher.finish()
    }
    let package_ver_hash = hash_version(pkg_version);
    format!("{name}-{package_ver_hash:x}")
}

/// A temporary directory that we can use for cloning a registry-sourced package's index file and discovering
/// the corresponding CID for that package.
///
/// The resulting directory is:
///
/// ```ignore
/// $HOME/.forc/registry/cache/tmp/<fetch_id>-name-<version_hash>
/// ```
///
/// A unique `fetch_id` may be specified to avoid contention over the registry directory in the
/// case that multiple processes or threads may be building different projects that may require
/// fetching the same dependency.
fn tmp_registry_package_dir(
    fetch_id: u64,
    name: &str,
    version: &semver::Version,
    namespace: &Namespace,
) -> PathBuf {
    let repo_dir_name = format!(
        "{:x}-{}",
        fetch_id,
        registry_package_dir_name(name, version)
    );
    cache_dir(namespace).join("tmp").join(repo_dir_name)
}

impl source::Pin for Source {
    type Pinned = Pinned;
    fn pin(&self, ctx: source::PinCtx) -> anyhow::Result<(Self::Pinned, PathBuf)> {
        let pkg_name = ctx.name;
        let cid = futures::executor::block_on(async {
            with_tmp_fetch_index(ctx.fetch_id(), pkg_name, self, |index_file| {
                let version = &self.version;
                let pkg_entry = index_file
                    .get(&version)
                    .ok_or_else(|| anyhow!("No {} found for {}", version, pkg_name))?;
                let cid = Cid::from_str(&pkg_entry.source_cid);
                Ok(cid)
            })
            .await
        })??;
        let path = pkg_cache_dir(&self.namespace, pkg_name, &self.version);
        let pinned = Pinned {
            source: self.clone(),
            cid,
        };
        Ok((pinned, path))
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

async fn with_tmp_fetch_index<F, O>(
    fetch_id: u64,
    pkg_name: &str,
    source: &Source,
    f: F,
) -> anyhow::Result<O>
where
    F: FnOnce(IndexFile) -> anyhow::Result<O>,
{
    let tmp_dir = tmp_registry_package_dir(fetch_id, pkg_name, &source.version, &source.namespace);
    if tmp_dir.exists() {
        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    // TODO: remove the clone
    let github_resolver = GithubRegistryResolver::with_default_github(source.namespace.clone());

    let path = format!(
        "{}",
        location_from_root(github_resolver.chunk_size, &source.namespace, pkg_name).display()
    );
    let index_repo_owner = github_resolver.repo_org;
    let index_repo_name = github_resolver.repo_name;
    let github_endpoint =
        format!("https://raw.githubusercontent.com/{index_repo_owner}/{index_repo_name}/{path}");

    let client = reqwest::Client::new();
    let pkg_entry = client
        .get(github_endpoint)
        .send()
        .await?
        .json::<IndexFile>()
        .await?;

    let res = f(pkg_entry)?;
    let _ = std::fs::remove_dir_all(&tmp_dir);
    Ok(res)
}
