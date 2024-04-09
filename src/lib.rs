//! # Pierport
//!
//! # Reference building blocks for the Pierport Protocol
//!
//! This library contains building blocks for a rust-based implementation of the pierport protocol.

use anyhow::anyhow;
use async_compression::futures::{bufread::ZstdDecoder, write::ZstdEncoder};
use async_tar::EntryType;
use async_trait::async_trait;
use async_zip::base::read;
use core::pin::{pin, Pin};
use futures::{
    io::{
        AsyncRead, AsyncReadExt, AsyncSeek, AsyncWrite, AsyncWriteExt, BufReader, BufWriter, Cursor,
    },
    StreamExt,
};
use log::*;
use octocrab::{
    models::{repos::Release, AssetId, ReleaseId},
    Octocrab,
};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime};
use tar::Archive;
use tokio::fs;
use tokio::process::Command;
use tokio_util::compat::TokioAsyncReadCompatExt;

pub mod header;

/// Supported import formats, as per pierport spec.
#[derive(Clone, Copy, Eq, PartialEq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportFormat {
    Zip,
    Zstd,
}

/// Import payload, as per pierport spec.
///
/// This is an alternative way to performing import requests - push a JSON payload that instructs
/// the import destination to pull the pier from alternative source. This has the potential to
/// minimize the number of intermediate copies needed.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ImportPayload {
    pub url: String,
    pub authorization: Option<String>,
    pub format: ImportFormat,
}

/// Status of the import session, as per pierport spec.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportStatus {
    /// Successfully imported
    Done,
    /// Failed to import
    Failed { reason: Option<String> },
    /// The pier is importing, status may contain extra information.
    Importing { status: Option<String> },
}

/// Crate-wide error type.
///
/// FIXME: this is a bit of a hack to simply wrap anyhow error, and we should split this into more
/// specific error enum, but this will do for now.
pub struct Error(anyhow::Error);

impl core::fmt::Debug for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        self.0.fmt(f)
    }
}

impl core::fmt::Display for Error {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        self.0.fmt(f)
    }
}

impl<T: Into<anyhow::Error>> From<T> for Error {
    fn from(e: T) -> Self {
        Self(e.into())
    }
}

impl axum::response::IntoResponse for Error {
    fn into_response(self) -> axum::response::Response {
        (
            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
            format!("{:#?}", self.0.to_string()),
        )
            .into_response()
    }
}

pub type Result<T> = core::result::Result<T, Error>;

#[async_trait]
pub trait AsyncUnpack {
    /// Unpack a file at given path.
    async fn unpack<'a>(
        &'a mut self,
        path: &'a Path,
        in_subpath: bool,
        stream: Pin<&'a mut (dyn AsyncRead + Send + 'a)>,
    ) -> io::Result<()>;

    /// Unpack an executable at given path.
    ///
    /// The only difference from `unpack` is that this expects the resulting file to be marked as
    /// executable.
    async fn unpack_exec<'a>(
        &'a mut self,
        path: &'a Path,
        in_subpath: bool,
        stream: Pin<&'a mut (dyn AsyncRead + Send + 'a)>,
    ) -> io::Result<()>;
}

#[async_trait]
impl<
        T: for<'a> Fn(&'a Path, bool, Pin<&'a mut (dyn AsyncRead + Send + 'a)>, bool) -> F + Send,
        F: core::future::Future<Output = io::Result<()>> + Send,
    > AsyncUnpack for T
{
    async fn unpack<'a>(
        &'a mut self,
        path: &'a Path,
        in_subpath: bool,
        stream: Pin<&'a mut (dyn AsyncRead + Send + 'a)>,
    ) -> io::Result<()> {
        let f = (*self)(path, in_subpath, stream, false);
        f.await
    }

    async fn unpack_exec<'a>(
        &'a mut self,
        path: &'a Path,
        in_subpath: bool,
        stream: Pin<&'a mut (dyn AsyncRead + Send + 'a)>,
    ) -> io::Result<()> {
        let f = (*self)(path, in_subpath, stream, true);
        f.await
    }
}

pub async fn find_files_by_suffix(
    directory: impl AsRef<Path>,
    suffix: &str,
) -> io::Result<Vec<fs::DirEntry>> {
    let mut entries = vec![];
    if let Ok(mut read_dir) = fs::read_dir(directory).await {
        while let Ok(Some(entry)) = read_dir.next_entry().await {
            let filetype = entry.file_type().await?;
            if filetype.is_dir() {
                continue;
            }
            if let Some(true) = entry.path().to_str().map(|s| s.ends_with(suffix)) {
                entries.push(entry)
            }
        }
    }
    Ok(entries)
}

/// Actions to be taken after pier is unpacked.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct PostUnpackCfg {
    /// Catch up the event log.
    prep: bool,
    /// Cram and verify that the cram hasn't changed after subsequent steps.
    verify_cram: bool,
    /// Run vere pack.
    pack: bool,
    /// Run vere meld.
    meld: bool,
    /// Run vere chop.
    chop: bool,
}

impl PostUnpackCfg {
    pub fn verify_cram(self, verify_cram: bool) -> Self {
        Self {
            verify_cram,
            ..self
        }
    }

    pub fn prep(self, prep: bool) -> Self {
        Self { prep, ..self }
    }

    pub fn pack(self, pack: bool) -> Self {
        Self { pack, ..self }
    }

    pub fn meld(self, meld: bool) -> Self {
        Self { meld, ..self }
    }

    pub fn chop(self, chop: bool) -> Self {
        Self { chop, ..self }
    }

    pub fn all() -> Self {
        Self {
            verify_cram: true,
            prep: true,
            pack: true,
            meld: true,
            chop: true,
        }
    }
}

#[derive(Clone)]
pub struct StandardUnpack<T: AsRef<Path>> {
    path: T,
    loom: Option<usize>,
}

impl<T: AsRef<Path>> StandardUnpack<T> {
    pub fn loom(&self) -> Option<usize> {
        self.loom
    }
}

impl<T: AsRef<Path>> Drop for StandardUnpack<T> {
    fn drop(&mut self) {
        let path = self.path.as_ref();
        debug!("Drop {path:?}");
        if path.exists() {
            if let Err(e) = std::fs::remove_dir_all(path) {
                error!("StandardUnpack: unable to remove dir ({e:?})");
            }
        }
    }
}

impl<T: AsRef<Path>> core::ops::Deref for StandardUnpack<T> {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        self.path.as_ref()
    }
}

impl<T: AsRef<Path>> StandardUnpack<T> {
    pub async fn new(path: T, loom: Option<usize>) -> Result<StandardUnpack<T>> {
        if path.as_ref().exists() {
            info!("Remove {:?}", path.as_ref());
            fs::remove_dir_all(path.as_ref()).await?;
        }

        fs::create_dir_all(path.as_ref()).await?;

        Ok(Self { path, loom })
    }

    pub async fn detect_loom(&mut self) -> Result<Option<usize>> {
        // TODO: autodetect loom based on the snapshot size
        Ok(None)
    }

    pub fn set_loom(&mut self, loom: Option<usize>) {
        self.loom = loom;
    }

    async fn run_cmd(&mut self, args: &[&str]) -> Result<()> {
        let mut cmd = Command::new("./.run");

        cmd.current_dir(&**self).args(args);

        if let Some(loom) = self.loom {
            cmd.args(["--loom", &loom.to_string(), "-t"]);
        }

        let output = cmd.output().await?;

        trace!("{:?}", std::str::from_utf8(&output.stdout));

        if !output.status.success() {
            Err(anyhow!(
                "Command failed: {} {:?}",
                output.status,
                std::str::from_utf8(&output.stderr)
            )
            .into())
        } else {
            Ok(())
        }
    }

    pub async fn cram(mut self) -> Result<StandardUnpack<T>> {
        debug!("Pre-work cram");
        self.run_cmd(&["cram"]).await?;
        Ok(self)
    }

    pub async fn verify_cram(mut self) -> Result<StandardUnpack<T>> {
        debug!("Post-work");

        // Get the path to current jam
        let roc_dir = self.join(".urb/roc");
        let entries = find_files_by_suffix(&roc_dir, ".jam").await?;

        let [entry] = &entries[..] else {
            return Err(anyhow!("Invalid number of jams").into());
        };

        let hash = sha256::async_digest::try_async_digest(entry.path()).await?;
        fs::remove_file(entry.path()).await?;

        debug!("Pre-work hash: {hash}");

        // Get the current hash of the current jam

        self.run_cmd(&["cram"]).await?;

        let hash2 = sha256::async_digest::try_async_digest(entry.path()).await?;
        fs::remove_dir_all(roc_dir).await?;

        debug!("Post-work hash: {hash}");

        if hash == hash2 {
            Ok(self)
        } else {
            Err(anyhow!("Pre and post work jam mismatch").into())
        }
    }

    pub async fn prep(mut self) -> Result<StandardUnpack<T>> {
        debug!("Prep");
        self.run_cmd(&["prep"]).await?;
        Ok(self)
    }

    pub async fn pack(mut self) -> Result<StandardUnpack<T>> {
        debug!("Pack");
        self.run_cmd(&["pack"]).await?;
        Ok(self)
    }

    pub async fn meld(mut self) -> Result<StandardUnpack<T>> {
        debug!("Meld");
        self.run_cmd(&["meld"]).await?;
        Ok(self)
    }

    pub async fn chop(mut self) -> Result<StandardUnpack<T>> {
        debug!("Chop");
        self.run_cmd(&["chop"]).await?;

        // Cleans up all pre-3.0 chops
        let chop_dir = self.join(".urb/log/chop");
        if chop_dir.exists() {
            fs::remove_dir_all(chop_dir).await?;
        }

        // Cleans up all 3.0+ chops
        // Remove all epochs, but the latest one
        let log_dir = self.join(".urb/log");
        let mut max_epoch = None;
        if let Ok(mut read_dir) = fs::read_dir(&log_dir).await {
            while let Ok(Some(entry)) = read_dir.next_entry().await {
                let filetype = entry.file_type().await?;
                if !filetype.is_dir() {
                    continue;
                }
                let fname = entry.file_name();
                let Some(fname) = fname.to_str() else {
                    continue;
                };

                let Some(epoch) = fname
                    .strip_prefix("0i")
                    .and_then(|v| v.parse::<usize>().ok())
                else {
                    continue;
                };

                match max_epoch {
                    None => {
                        max_epoch = Some(epoch);
                    }
                    Some(e) if e < epoch => {
                        fs::remove_dir_all(log_dir.join(&format!("0i{e}"))).await?;
                        max_epoch = Some(epoch);
                    }
                    // This branch should always be hit, but don't make it unconditional, just to
                    // be sure that we are not deleting the latest epoch.
                    Some(e) if e != epoch => {
                        fs::remove_dir_all(log_dir.join(&format!("0i{epoch}"))).await?;
                    }
                    Some(_) => (),
                }
            }
        }

        Ok(self)
    }

    pub async fn post_unpack(
        mut self,
        vere_version: VereVersion,
        db_path: &Path,
        cache_path: &Path,
        cfg: &PostUnpackCfg,
    ) -> Result<StandardUnpack<T>> {
        // We need to correct the vere architecture to something
        let db = VersionDb::load(db_path).await?;

        let vere = db.bin_from_version(&vere_version, cache_path).await?;

        let vere_path = self.join(".run");
        fs::write(&vere_path, vere).await?;

        let mut perms = fs::metadata(&vere_path).await?.permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&vere_path, perms).await?;

        if cfg.prep {
            self = self.prep().await?;
        }

        if cfg.verify_cram {
            self = self.cram().await?;
        }

        if cfg.pack {
            self = self.pack().await?;
        }

        if cfg.meld {
            self = self.meld().await?;
        }

        if cfg.chop {
            self = self.chop().await?;
        }

        if cfg.verify_cram {
            self = self.verify_cram().await?;
        }

        Ok(self)
    }

    pub async fn lmdb_patp(self) -> Result<String> {
        Err(anyhow!("Not yet implemented").into())
    }
}

#[async_trait]
impl<T: AsRef<Path> + Send> AsyncUnpack for StandardUnpack<T> {
    async fn unpack<'a>(
        &'a mut self,
        path: &'a Path,
        in_subpath: bool,
        stream: Pin<&'a mut (dyn AsyncRead + Send + 'a)>,
    ) -> io::Result<()> {
        let path = if in_subpath {
            path.components()
                .skip_while(|c| c == &Component::CurDir)
                .skip(1)
                .collect()
        } else {
            path.to_path_buf()
        };

        let dir = if let Some(parent) = path.parent() {
            self.join(parent)
        } else {
            self.to_path_buf()
        };

        trace!("Write file {path:?} {in_subpath} {dir:?}");

        fs::create_dir_all(dir).await?;

        let out_path = self.join(&path);

        let file = fs::File::create(&out_path).await?;

        futures::io::copy(stream, &mut file.compat()).await?;

        trace!("Written file");

        Ok(())
    }

    async fn unpack_exec<'a>(
        &'a mut self,
        path: &'a Path,
        in_subpath: bool,
        stream: Pin<&'a mut (dyn AsyncRead + Send + 'a)>,
    ) -> io::Result<()> {
        self.unpack(path, in_subpath, stream).await?;

        let path = if in_subpath {
            path.components()
                .skip_while(|c| c == &Component::CurDir)
                .skip(1)
                .collect()
        } else {
            path.to_path_buf()
        };

        let path = self.join(&path);

        trace!("Metadata {path:?}");

        let mut perms = fs::metadata(&path).await?.permissions();
        perms.set_mode(0o755);

        trace!("Set perms {path:?}");

        fs::set_permissions(&path, perms).await?;

        trace!("Unpacked");

        Ok(())
    }
}

// 16GB
const MAX_SIZE: u64 = 0x400000000;
// 128MB
const MAX_VERE_SIZE: u64 = 0x8000000;

#[derive(Debug)]
pub enum Pace {
    Live,
    Once,
}

#[derive(Default, Debug)]
pub struct UrbitPier {
    pub pace: Option<Pace>,
    pub vere_hash: Option<String>,
    // Vere version hash matched against database of whitelisted runtimes
    //pub vere_version: Option<String>,
}

impl UrbitPier {
    // Attempts to match the pier's vere hash with the officially released vere.
    //
    // If the pier has no vere binary, and undocked_fallback specifies target OS and arch, then
    // this will take the latest vere release that is for the given os-arch pair.
    pub async fn vere_version(
        &self,
        db_path: &Path,
        undocked_fallback: Option<(&str, &str)>,
    ) -> Result<VereVersion> {
        let db = VersionDb::load(db_path).await.unwrap_or_default();
        // Refresh if database is older than 1 hour
        let (db, updated) = db.refresh_if_older(Duration::from_secs(3600)).await?;

        if updated {
            db.save(db_path).await?;
        }

        if let Some(vere_hash) = self.vere_hash.as_ref() {
            db.versions
                .get(vere_hash)
                .map(|v| v.inner.clone())
                .ok_or_else(|| anyhow!("Could not match vere hash with version").into())
        } else if let Some((os, arch)) = undocked_fallback {
            debug!("No vere hash found. Falling back to latest.");
            db.versions
                .values()
                .fold(None, |prev: Option<&VereVersion>, cur| {
                    if cur.inner.os == os && cur.inner.arch == arch {
                        if let Some(prev) = prev {
                            if prev
                                .version
                                .split('.')
                                .map(|v| v.parse::<u32>().unwrap_or_default())
                                .cmp(
                                    cur.inner
                                        .version
                                        .split('.')
                                        .map(|v| v.parse::<u32>().unwrap_or_default()),
                                )
                                == core::cmp::Ordering::Less
                            {
                                return Some(&cur.inner);
                            }
                        } else {
                            return Some(&cur.inner);
                        }
                    }
                    prev
                })
                .cloned()
                .ok_or_else(|| anyhow!("Could not find a version for given arch").into())
        } else {
            Err(anyhow!("No vere in pier, and no undocked fallback set").into())
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct VereVersion {
    pub version: String,
    pub os: String,
    pub arch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct VereVersionOuter {
    inner: VereVersion,
    asset: AssetId,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
pub struct VersionDb {
    // HashMap would be more efficient, but the map is not going to be huge anyways, and sorting
    // everything would be better.
    versions: BTreeMap<String, VereVersionOuter>,
    latest_release: Option<ReleaseId>,
    processed_assets: BTreeSet<AssetId>,
    update_time: Option<SystemTime>,
}

impl VersionDb {
    pub async fn load(path: &Path) -> Result<Self> {
        let data = fs::read_to_string(path).await?;
        serde_json::from_str(&data).map_err(Into::into)
    }

    async fn get_with_redirects(gh: &Octocrab, mut url: String) -> Result<Vec<u8>> {
        let mut cnt = 0;

        loop {
            cnt += 1;

            if cnt > 10 {
                return Err(anyhow!("Too many redirects").into());
            }

            let response = gh._get(&url).await?;

            if response.status() != hyper::StatusCode::OK {
                let Some(location) = response.headers().get(hyper::header::LOCATION) else {
                    return Err(anyhow!("Error downloading vere: {}", response.status()).into());
                };
                url = location.to_str()?.to_string();
                debug!("Redirect to {url}");
            } else {
                break Ok(hyper::body::to_bytes(response.into_body()).await?.to_vec());
            }
        }
    }

    async fn ensure_downloaded(&self, id: AssetId, cache_dir: &Path) -> Result<()> {
        fs::create_dir_all(cache_dir).await?;

        let asset_path = cache_dir.join(id.to_string());

        // Check if there's existing asset with correct hash
        if let Ok(bytes) = fs::read(&asset_path).await {
            let digest = sha256::digest(&bytes);

            if self.versions.get(&digest).map(|v| v.asset == id) == Some(true) {
                debug!("Cached vere version found for {id}.");
                return Ok(());
            }

            debug!("Hash mismatch for {id}. Redownloading.");
        }

        let gh = octocrab::instance();
        let repos = gh.repos("urbit", "vere");
        let releases = repos.releases();
        let asset = releases.get_asset(id).await?;

        let url = asset.browser_download_url.to_string();
        debug!("Download vere from {url}");
        let bytes = Self::get_with_redirects(&gh, url).await?;

        let bytes = tokio::task::spawn_blocking(move || Self::unpack_vere(bytes))
            .await??
            .1;

        fs::write(asset_path, bytes).await?;

        Ok(())
    }

    async fn bin_from_version(&self, version: &VereVersion, cache_dir: &Path) -> Result<Vec<u8>> {
        let asset = self
            .versions
            .values()
            .find_map(|v| {
                if &v.inner == version {
                    Some(v.asset)
                } else {
                    None
                }
            })
            .ok_or_else(|| anyhow!("Could not find compatible binary asset"))?;

        self.ensure_downloaded(asset, cache_dir).await?;

        Ok(fs::read(cache_dir.join(asset.to_string())).await?)
    }

    fn unpack_vere(bytes: impl AsRef<[u8]>) -> Result<(String, Vec<u8>)> {
        use std::io::Read;

        let gz = flate2::read::GzDecoder::new(bytes.as_ref());
        let mut a = Archive::new(gz);

        // We expect only a single file here
        let mut f = a
            .entries()?
            .next()
            .transpose()?
            .ok_or_else(|| anyhow!("File empty"))?;

        let mut bytes = vec![];
        f.read_to_end(&mut bytes)?;

        // Compute hash of the executable
        Ok((sha256::digest(&bytes), bytes))
    }

    async fn process_release(&mut self, gh: &Octocrab, release: Release) -> Result<()> {
        self.latest_release = Some(release.id);

        let Some(version) = release.tag_name.strip_prefix("vere-v") else {
            warn!(
                "Skipping {}, because it has invalid prefix.",
                release.tag_name
            );
            return Ok(());
        };

        for asset in release.assets {
            // Only process tgz archives
            let Some(archive) = asset.name.strip_suffix(".tgz") else {
                continue;
            };
            let Some((os, arch)) = archive
                .split_once('-')
                .map(|(a, b)| (a.to_string(), b.to_string()))
            else {
                warn!("Skipping {archive}, since it is not formatted as OS-Arch");
                continue;
            };

            if self.processed_assets.contains(&asset.id) {
                continue;
            }

            // Now, pull the binary
            debug!("Downloading {version}: {archive}");

            let bytes =
                Self::get_with_redirects(gh, asset.browser_download_url.to_string()).await?;

            let inner = VereVersion {
                version: version.into(),
                os,
                arch,
            };

            let hash = tokio::task::spawn_blocking(move || Self::unpack_vere(bytes))
                .await??
                .0;

            self.versions.insert(
                hash.clone(),
                VereVersionOuter {
                    inner,
                    asset: asset.id,
                },
            );

            self.processed_assets.insert(asset.id);
        }

        Ok(())
    }

    pub async fn refresh_if_older(self, duration: Duration) -> Result<(Self, bool)> {
        if self
            .update_time
            .and_then(|v| v.elapsed().ok())
            .map(|v| v >= duration)
            != Some(false)
        {
            self.refresh().await
        } else {
            Ok((self, false))
        }
    }

    pub async fn refresh(mut self) -> Result<(Self, bool)> {
        let gh = octocrab::instance();

        let repos = gh.repos("urbit", "vere");
        let releases = repos.releases();

        // Fetch latest urbit release
        let latest = releases.get_latest().await?;

        self.update_time = Some(SystemTime::now());

        if Some(latest.id) == self.latest_release {
            info!("Only syncing latest release ({})", latest.tag_name);

            // process assets of the latest release to make sure any asset changes are synced up.
            self.process_release(&gh, latest).await?;
            return Ok((self, false));
        }

        info!("Pulling new vere releases");

        // If we have a mismatch of the latest release, pull all releases, sort them by created_at
        // attribute, and update our ID.

        let mut out_rel = vec![];

        for i in 0u32.. {
            // Pull 10 releases at a time, to not hit timeout conditions
            let Ok(list) = releases.list().per_page(5).page(i).send().await else {
                break;
            };

            if list.items.is_empty() {
                break;
            }

            let incomplete = list.incomplete_results == Some(true);

            let mut hit_latest = false;

            out_rel.extend(
                list.into_iter()
                    .filter(|v| !v.draft && !v.prerelease)
                    .inspect(|v| hit_latest = hit_latest || Some(v.id) == self.latest_release),
            );

            // We hit the latest release we have at the moment. We assume further releases will be
            // old ones, so that we do not have to
            if hit_latest {
                break;
            }

            if incomplete {
                return Err(anyhow!(
                    "Got incomplete results, we do not support that at the moment"
                )
                .into());
            }
        }

        out_rel.sort_by_key(|v| v.created_at);

        for release in out_rel {
            debug!("Process {}", release.tag_name);
            self.process_release(&gh, release).await?;
        }

        Ok((self, true))
    }

    pub async fn save(&self, path: &Path) -> Result<()> {
        let data = serde_json::to_string_pretty(self)?;
        fs::write(path, data.as_bytes()).await?;
        Ok(())
    }
}

/// Returns (is_subpath, has_subcomponents, in_subpath)
fn is_subpath(in_path: &Path, target_path: &Path) -> Option<(bool, bool)> {
    let mut in_components = in_path.components().skip_while(|c| c == &Component::CurDir);

    let target_components = target_path
        .components()
        .skip_while(|c| c == &Component::CurDir);

    let mut in_subpath = false;

    for target in target_components {
        // We may need to get 2 input components out given 1 target component.
        loop {
            let Some(inp) = in_components.next() else {
                return None;
            };

            if target != inp {
                if in_subpath {
                    return None;
                } else {
                    in_subpath = true;
                    continue;
                }
            }

            // Intentionally break in all cases except 1 - the control flow is easier to reason
            // about this way.
            break;
        }
    }

    Some((in_components.next().is_some(), in_subpath))
}

fn is_path(in_path: &Path, target_path: &Path) -> (bool, bool) {
    is_subpath(in_path, target_path)
        .map(|(a, b)| if !a { (true, b) } else { (false, false) })
        .unwrap_or_default()
}

fn is_vere(in_path: &Path) -> (bool, bool) {
    is_path(in_path, Path::new(".run"))
}

fn is_allowed_file(in_path: &Path) -> (bool, bool) {
    for f in [".bin/pace", ".run"] {
        if let Some((false, in_subpath)) = is_subpath(in_path, Path::new(f)) {
            return (true, in_subpath);
        }
    }

    // Exclude any of the following, because we simply don't need them
    for d in [".urb/get", ".urb/put", ".urb/roc"] {
        if is_subpath(in_path, Path::new(d)).is_some() {
            return (false, false);
        }
    }

    for d in [".urb"] {
        if let Some((true, in_subpath)) = is_subpath(in_path, Path::new(d)) {
            return (true, in_subpath);
        }
    }

    (false, false)
}

/// Filters files on a zstd stream, and outputs them to given function
pub async fn import_zstd_stream<I: AsyncRead + Send>(
    stream_in: I,
    file_out: &mut (impl AsyncUnpack + ?Sized),
) -> Result<UrbitPier> {
    import_zstd_stream_with_vere(stream_in, file_out, false).await
}

/// Filters files on a zstd stream, and outputs them to given function
pub async fn import_zstd_stream_with_vere<I: AsyncRead + Send>(
    stream_in: I,
    file_out: &mut (impl AsyncUnpack + ?Sized),
    unpack_vere: bool,
) -> Result<UrbitPier> {
    let stream_in = BufReader::new(stream_in);
    let mut stream_in = ZstdDecoder::new(stream_in);
    let stream_in = pin!(stream_in);
    let ar = async_tar::Archive::new(stream_in);
    let mut entries = ar.entries()?;

    // This allows us to be consistent with parsing paths from `./<patp>/` and `./`
    let mut subpath_mode = None;

    let mut pier = UrbitPier::default();

    debug!("IMPORT ZSTD");

    while let Some(entry) = entries.next().await {
        let mut entry = entry?;

        debug!("ZSTD {entry:?}");

        if !matches!(
            entry.header().entry_type(),
            EntryType::Regular
                | EntryType::Continuous
                | EntryType::GNULongName
                | EntryType::GNUSparse
        ) {
            debug!("CONTINUE");
            continue;
        }

        let path: PathBuf = (*entry.path()?).into();
        let size = entry.header().size()?;

        let (is_vere, in_subpath) = is_vere(&path);

        debug!("ZSTD: {path:?} is_vere={is_vere} in_subpath={in_subpath}");

        if is_vere {
            if subpath_mode.is_some() && Some(in_subpath) != subpath_mode {
                warn!(
                    "Subpath mode does not match ({in_subpath} vs. {})",
                    subpath_mode.unwrap()
                );
                continue;
            } else {
                subpath_mode = Some(in_subpath);

                if size > MAX_VERE_SIZE {
                    warn!("Vere too large ({size} bytes)",);
                    continue;
                }

                // We need to read vere out, to compute its hash, and skip output, because we will
                // write vere properly later with correct architecture.
                let mut vere = vec![];
                entry.read_to_end(&mut vere).await?;
                pier.vere_hash = Some(sha256::digest(&vere));

                if unpack_vere {
                    file_out
                        .unpack_exec(&path, in_subpath, pin!(Cursor::new(vere)))
                        .await?;
                }

                continue;
            }
        }

        let (is_allowed_file, in_subpath) = is_allowed_file(&path);

        if is_allowed_file {
            if subpath_mode.is_some() && Some(in_subpath) != subpath_mode {
                warn!(
                    "Subpath mode does not match ({in_subpath} vs. {})",
                    subpath_mode.unwrap()
                );
                continue;
            } else {
                subpath_mode = Some(in_subpath);

                if size > MAX_SIZE {
                    warn!("File too large ({size} bytes)",);
                    continue;
                }

                file_out.unpack(&path, in_subpath, pin!(entry)).await?;
            }
        }
    }

    if subpath_mode.is_some() {
        Ok(pier)
    } else {
        Err(anyhow!("Pier is empty").into())
    }
}

/// Filters files on a zip file, and outputs them to a given function
///
/// Note that for zip to work, we need to have a seekable stream, i.e. file.
pub async fn import_zip_file<I: AsyncRead + AsyncSeek + Send>(
    stream_in: I,
    file_out: &mut (impl AsyncUnpack + ?Sized),
) -> Result<UrbitPier> {
    let mut stream_in = BufReader::new(stream_in);
    let stream_in = pin!(stream_in);
    let mut zip = read::seek::ZipFileReader::new(stream_in).await?;

    // This allows us to be consistent with parsing paths from `./<patp>/` and `./`
    let mut subpath_mode = None;

    let mut pier = UrbitPier::default();

    for i in 0.. {
        let Ok(mut file) = zip.reader_with_entry(i).await else {
            break;
        };

        // This is not a file, we don't need it
        if matches!(file.entry().dir(), Ok(true)) {
            continue;
        }

        let entry = file.entry();

        let Ok(path) = entry.filename().as_str().map(PathBuf::from) else {
            continue;
        };

        let (is_vere, in_subpath) = is_vere(&path);

        if is_vere {
            if subpath_mode.is_some() && Some(in_subpath) != subpath_mode {
                warn!(
                    "Subpath mode does not match ({in_subpath} vs. {})",
                    subpath_mode.unwrap()
                );
                continue;
            } else {
                subpath_mode = Some(in_subpath);

                if file.entry().uncompressed_size() > MAX_VERE_SIZE {
                    warn!(
                        "Vere too large ({} bytes)",
                        file.entry().uncompressed_size()
                    );
                    continue;
                }

                // We need to read vere out, to compute its hash, and skip output, because we will
                // write vere properly later with correct architecture.
                let mut vere = vec![];
                file.read_to_end_checked(&mut vere).await?;
                pier.vere_hash = Some(sha256::digest(&vere));
                continue;
            }
        }

        let (is_allowed_file, in_subpath) = is_allowed_file(&path);

        if is_allowed_file {
            if subpath_mode.is_some() && Some(in_subpath) != subpath_mode {
                warn!(
                    "Subpath mode does not match ({in_subpath} vs. {})",
                    subpath_mode.unwrap()
                );
                continue;
            } else {
                subpath_mode = Some(in_subpath);

                if file.entry().uncompressed_size() > MAX_SIZE {
                    warn!(
                        "File too large ({} bytes)",
                        file.entry().uncompressed_size()
                    );
                    continue;
                }

                file_out.unpack(&path, in_subpath, pin!(file)).await?;
            }
        }
    }

    Ok(pier)
}

/// Compresses a pier directory using zstd compression algorithm.
pub async fn export_dir(path: &Path, stream_out: (impl AsyncWrite + Send + Sync)) -> Result<()> {
    let stream_out = BufWriter::new(stream_out);
    let stream_out = ZstdEncoder::new(stream_out);
    let stream_out = pin!(stream_out);

    let mut ar = async_tar::Builder::new(stream_out);
    ar.append_dir_all(".", path).await?;
    ar.finish().await?;
    let mut zstd = ar.into_inner().await?;
    zstd.close().await?;
    zstd.get_pin_mut().flush().await?;

    Ok(())
}

pub fn current_os_arch() -> (&'static str, &'static str) {
    use std::env::consts::{ARCH, OS};
    (OS, ARCH)
}
