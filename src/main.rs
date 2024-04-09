use clap::Parser;
use core::pin::Pin;
use futures::{
    io::{AsyncRead, AsyncReadExt},
    TryStreamExt,
};
use jsonwebtoken::{Algorithm, DecodingKey, Validation};
use log::*;
use once_cell::sync::Lazy;
use pierport::*;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::fs::OpenOptions;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::{compat::TokioAsyncReadCompatExt, io::ReaderStream};

use axum::{
    extract::{DefaultBodyLimit, Json, Path, State},
    response::IntoResponse,
    routing::{get, post},
    Router,
};
use hyper::{Request, Response, StatusCode};
use reqwest::Client;

static CS: Lazy<Mutex<()>> = Lazy::new(Default::default);

static CLIENT: Lazy<Client> = Lazy::new(Client::new);

#[derive(Serialize, Deserialize)]
#[serde(default)]
struct Config {
    /// Staging directory for imported piers.
    ///
    /// This directory stores and unpacks imported piers, before proxying them.
    ///
    /// The staging dir assumes unprocessed piers, so any files found there are assumed to be
    /// deletable.
    staging_dir: PathBuf,
    /// Directory where imported piers are moved, before being proxied.
    ///
    /// When piers are proxied over, the import request already returns ACCEPTED.
    imported_dir: PathBuf,
    /// Where to invoke ship import after cleaning up pier data.
    ///
    /// The endpoint must be the base import address that speaks the pierport protocol.
    ///
    /// For instance:
    ///
    /// `http://internal-import.ts.infra.net/api/port/import`.
    ///
    /// If the value is supplied, then, pierport will remove the pier from `imported_dir`, after
    /// successful output from the proxy base. If the value is unspecified, or proxy returns an
    /// error, then pier directory will remain on the machine.
    proxy_import_base: Option<String>,
    /// Path to vere database json.
    db_path: PathBuf,
    /// Path to vere binary cache.
    cache_dir: PathBuf,
    /// Where to bind the pierport server on.
    bind: SocketAddr,
    /// Maximum loom argument for cleanup tasks.
    ///
    /// If set, the importer will start listening to `Pierport-Loom` header, and clamp it to the
    /// given value. Otherwise, the header is ignored.
    ///
    /// The size is given in bits, same as in vere.
    max_loom: Option<usize>,
    /// Limit for uploaded pier archives.
    ///
    /// Note that this does not consider the size of the uncompressed pier, instead, only considers
    /// the compressed size.
    upload_limit: usize,
    /// Cleanup steps to run after the pier has been unpacked.
    ///
    /// Defaults to all steps, unless there is a section in the config negating it (sufficient to
    /// declare empty post_unpack section).
    post_unpack: PostUnpackCfg,
    /// How often should we cleanup complete sessions.
    session_check_interval_seconds: u64,
    /// How old should completed sessions be before being removed.
    session_remove_time_seconds: u64,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            staging_dir: "./pierport_staging/".into(),
            imported_dir: "./pierport_imported/".into(),
            proxy_import_base: None,
            db_path: "./pierport_vere_db.json".into(),
            cache_dir: "./pierport_vere_cache/".into(),
            bind: "0.0.0.0:4242".parse().unwrap(),
            max_loom: None,
            // 4G
            upload_limit: 0x100000000,
            post_unpack: PostUnpackCfg::all(),
            session_check_interval_seconds: 60,
            session_remove_time_seconds: 600,
        }
    }
}

/// Pierport - urbit pier import service
///
/// ## Ensuring authentication
///
/// If a Json Web Token secret is specified in "JWT_AUTH_SECRET" environment variable, then all
/// incoming requests will need to have a valid Authorization header that contains a jwt,
/// verifiable by the given secret.
///
/// The env var is interpreted as raw bytes, rather than a base64 encoded string.
#[derive(Parser)]
#[command(author, version, about)]
#[command(propagate_version = false)]
struct Args {
    #[arg(short, long)]
    config: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init_from_env(env_logger::Env::default().default_filter_or("pierport=info"));

    let Args { config } = Args::parse();

    // JWT secret is used to verify pier import requests.
    let jwt_key = std::env::var_os("JWT_AUTH_SECRET");

    let jwt_key = if let Some(secret) = jwt_key.as_deref().and_then(|v| v.to_str()) {
        Some(Arc::new(DecodingKey::from_secret(secret.as_bytes())))
    } else {
        warn!("No JWT_AUTH_SECRET specified - authentication is disabled!");
        None
    };

    let config: Config = if let Some(config) = config {
        let bytes = tokio::fs::read_to_string(config).await?;
        toml::from_str(&bytes)?
    } else {
        Default::default()
    };

    let config = Arc::new(config);

    let sessions = Arc::new(Sessions::default());

    // Cleanup stale/completed sessions at fixed interval
    tokio::spawn({
        let sessions = sessions.clone();
        let config = config.clone();
        async move {
            loop {
                tokio::time::sleep(Duration::from_secs(config.session_check_interval_seconds))
                    .await;
                sessions
                    .cleanup_stale_sessions(Duration::from_secs(config.session_remove_time_seconds))
                    .await;
            }
        }
    });

    // TODO: should we include pierport/v1/ here?
    let app = Router::new()
        .route("/import/~:patp", post(import))
        .route("/import/~:patp/:id", get(import_status))
        .layer(DefaultBodyLimit::max(config.upload_limit))
        .with_state((config.clone(), jwt_key, sessions));

    axum::Server::bind(&config.bind)
        .serve(app.into_make_service())
        .await?;

    Ok(())
}

/// Check on the pier import status.
async fn import_status(
    State((_, _, sessions)): State<(Arc<Config>, Option<Arc<DecodingKey>>, Arc<Sessions>)>,
    Path((patp, id)): Path<(String, u64)>,
) -> axum::response::Result<(StatusCode, Json<ImportStatus>)> {
    debug!("Get status {patp} {id}");
    let status = match sessions.get_session_status(&patp, id).await? {
        Some(Ok(())) => ImportStatus::Done,
        Some(Err(e)) => ImportStatus::Failed {
            reason: Some(e.to_string()),
        },
        None => ImportStatus::Importing { status: None },
    };

    Ok((StatusCode::OK, Json(status)))
}

/// The main pier import routine.
async fn import(
    State((config, jwt_key, sessions)): State<(
        Arc<Config>,
        Option<Arc<DecodingKey>>,
        Arc<Sessions>,
    )>,
    Path(patp): Path<String>,
    req: Request<hyper::body::Body>,
) -> axum::response::Result<(StatusCode, String)> {
    let Config {
        staging_dir,
        imported_dir,
        ..
    } = &*config;

    let patp: Arc<str> = Arc::from(patp);

    // If we have a jwt secret, then use it to verify the requests
    if let Some(secret) = jwt_key {
        let jwt = req
            .headers()
            .get(hyper::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .ok_or((StatusCode::UNAUTHORIZED, "JWT authorization missing"))?;

        let jwt = jsonwebtoken::decode::<Claims>(jwt, &secret, &Validation::new(Algorithm::HS256))
            .map_err(|e| {
                error!("{e:?}");
                (StatusCode::UNAUTHORIZED, "Invalid JWT supplied")
            })?;

        if jwt.claims.sub != &*patp {
            debug!(
                "Mismatched subject provided ({} vs. {})",
                jwt.claims.sub, patp
            );
            return Err((StatusCode::UNAUTHORIZED, "Invalid JWT supplied").into());
        }
    }

    let out_dir = staging_dir.join(&*patp);

    // This critical section ensures that we cannot attempt to import twice to the same dir.
    let guard = CS.lock().await;

    // Due to the given order we do not risk toctou - dir rename operation is atomic.

    if out_dir.exists() {
        return Err((
            StatusCode::CONFLICT,
            "Pier already being imported".to_string(),
        )
            .into_response()
            .into());
    }

    let imported_out_dir = imported_dir.join(&*patp);

    if imported_out_dir.exists() {
        return Err((StatusCode::CONFLICT, "Pier already imported".to_string())
            .into_response()
            .into());
    }

    debug!("Got request: {out_dir:?}");
    let mut pier = StandardUnpack::new(out_dir.clone(), None).await?;

    // Drop the critical section so we can have multiple imports at the same time
    core::mem::drop(guard);

    let (parts, body) = req.into_parts();

    let mut stream: Pin<Box<dyn AsyncRead + Send>> = Box::pin(
        body.map_err(|v| {
            debug!("ERROR: {v:?}");
            std::io::Error::from(std::io::ErrorKind::UnexpectedEof)
        })
        .into_async_read(),
    );

    let mut content_type = parts
        .headers
        .get(hyper::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok());

    let info = loop {
        match content_type {
            Some("application/zip") => {
                let mut temp_zip = out_dir.clone();
                temp_zip.set_extension("zip");

                let mut temp_file = OpenOptions::new()
                    .read(true)
                    .write(true)
                    .create(true)
                    .truncate(true)
                    .open(&temp_zip)
                    .await
                    .map_err(Error::from)?;

                // Immediately unlink the file - it will still be written to the filesystem, but it
                // will be automatically released after the file is fully closed.
                tokio::fs::remove_file(temp_zip)
                    .await
                    .map_err(Error::from)?;

                futures::io::copy(stream, &mut (&mut temp_file).compat())
                    .await
                    .map_err(Error::from)?;

                // TODO: consider returning 202 before extracting the pier
                break import_zip_file(temp_file.compat(), &mut pier).await?;
            }
            Some("application/zstd") => break import_zstd_stream(stream, &mut pier).await?,
            Some("application/json") => {
                content_type = {
                    let mut data = String::new();

                    stream
                        .read_to_string(&mut data)
                        .await
                        .map_err(Error::from)?;

                    let payload: ImportPayload =
                        serde_json::from_str(&data).map_err(Error::from)?;

                    let mut req = CLIENT.get(&payload.url);

                    if let Some(auth) = payload.authorization {
                        req = req.header(hyper::header::AUTHORIZATION, auth);
                    }

                    let resp = req.send().await.map_err(Error::from)?;
                    let resp = resp.error_for_status().map_err(Error::from)?;

                    stream = Box::pin(
                        resp.bytes_stream()
                            .map_err(|v| {
                                debug!("ERROR: {v:?}");
                                std::io::Error::from(std::io::ErrorKind::UnexpectedEof)
                            })
                            .into_async_read(),
                    );

                    Some(match payload.format {
                        ImportFormat::Zip => "application/zip",
                        ImportFormat::Zstd => "application/zstd",
                    })
                }
            }
            _ => {
                return Err(Response::builder()
                    .status(StatusCode::NOT_ACCEPTABLE)
                    .body("Only zip or zstd tar streams are accepted".to_string())
                    .unwrap()
                    .into())
            }
        }
    };

    // past this point we can return 202 ACCEPTED and move to another thread.
    let session_id = sessions.alloc_session_id();

    let jh = tokio::spawn({
        let patp = patp.clone();
        let sessions = sessions.clone();
        async move {
            let ret = if let Err(e) = finish_import(
                patp.clone(),
                info,
                pier,
                out_dir,
                imported_out_dir,
                config.clone(),
            )
            .await
            {
                error!("Could not finish import: {e:?}");
                sessions.finish_session(patp, session_id, Err(e)).await
            } else {
                debug!("Import finished");
                sessions.finish_session(patp, session_id, Ok(())).await
            };

            if let Err(e) = ret {
                error!("Could not finish the session: {e:?}");
            }
        }
    });

    sessions.new_session(patp, session_id, jh).await;

    Ok((StatusCode::ACCEPTED, session_id.to_string()))
}

/// Asynchronous import, run under specific session ID.
async fn finish_import(
    patp: Arc<str>,
    info: UrbitPier,
    mut pier: StandardUnpack<PathBuf>,
    out_dir: PathBuf,
    imported_out_dir: PathBuf,
    config: Arc<Config>,
) -> Result<()> {
    let Config {
        db_path,
        cache_dir,
        imported_dir,
        ..
    } = &*config;

    debug!("{patp} - {info:?}");

    let (os, arch) = current_os_arch();
    debug!("{patp} - {os} {arch}");
    let version = info.vere_version(db_path, Some((os, arch))).await?;

    debug!("{patp} - {version:?}");

    let loom = pier.detect_loom().await?;

    if config
        .max_loom
        .zip(loom)
        .map(|(a, b)| a > b)
        .unwrap_or(false)
    {
        return Err(anyhow::anyhow!("Detected loom too large").into());
    }

    pier.set_loom(loom);

    let pier = pier
        .post_unpack(
            VereVersion {
                os: os.to_string(),
                arch: arch.to_string(),
                ..version
            },
            db_path,
            cache_dir,
            &config.post_unpack,
        )
        .await?;

    tokio::fs::create_dir_all(imported_dir)
        .await
        .map_err(Error::from)?;

    tokio::fs::rename(out_dir, &imported_out_dir)
        .await
        .map_err(Error::from)?;

    if let Some(proxy) = config.proxy_import_base.as_deref() {
        // 16MB buffer
        let (tx, rx) = tokio::io::duplex(0x1000000);

        let proxy_url = format!("{proxy}/~{patp}");
        let mut req = CLIENT.post(&proxy_url);

        req = req
            .header(hyper::header::CONTENT_TYPE, "application/zstd")
            .body(reqwest::Body::wrap_stream(ReaderStream::new(rx)));

        let export = export_dir(&imported_out_dir, tx.compat());

        let (a, b) = futures::join! {
            export,
            req.send()
        };

        a?;
        let resp = b?;

        if let Err(e) = resp.error_for_status_ref() {
            error!("Error importing: {e:?}");
            let text = resp.text().await;
            error!("Import error text: {text:?}");
            return Err(e.into());
        } else {
            // Poll the downstream source to completion.
            if resp.status() == hyper::StatusCode::ACCEPTED {
                let id = resp.text().await?;
                let poll_url = format!("{proxy_url}/{id}");
                loop {
                    let req = CLIENT.get(&poll_url);
                    if let Ok(resp) = req.send().await {
                        if let Err(e) = resp.error_for_status_ref() {
                            error!("Error polling for downstream completion: {e:?}");
                            let text = resp.text().await;
                            error!("Import poll error text: {text:?}");
                            return Err(e.into());
                        }

                        let data = resp.json::<ImportStatus>().await?;

                        match data {
                            ImportStatus::Done => break,
                            ImportStatus::Failed { reason } => {
                                error!("Error in downstream import: {reason:?}");
                                return Err(Error::from(anyhow::anyhow!(
                                    "Downstream failed to import: {reason:?}"
                                )));
                            }
                            ImportStatus::Importing { .. } => (),
                        }
                    }

                    // Do half a minute, because 60 seconds is how long the result must be held
                    // according to pierport spec.
                    // TODO: this should probably be configurable.
                    tokio::time::sleep(Duration::from_secs(30)).await;
                }
            }

            tokio::fs::remove_dir_all(imported_out_dir).await?;
        }
    }

    core::mem::drop(pier);

    Ok(())
}

// Auxiliary structures

/// Required claims used to authenticate JWT
#[derive(Debug, Serialize, Deserialize)]
struct Claims {
    iss: Option<String>,
    iat: Option<usize>,
    exp: usize,
    sub: String,
}

struct Session {
    result: core::result::Result<(Result<()>, Instant), JoinHandle<()>>,
}

impl Session {
    fn new(jh: JoinHandle<()>) -> Self {
        Self { result: Err(jh) }
    }

    fn finish_session(&mut self, res: Result<()>) {
        self.result = Ok((res, Instant::now()));
    }
}

#[derive(Default)]
struct Sessions {
    cnt: AtomicU64,
    sessions: Mutex<HashMap<Arc<str>, BTreeMap<u64, Session>>>,
}

impl Sessions {
    fn alloc_session_id(&self) -> u64 {
        self.cnt.fetch_add(1, Ordering::Relaxed)
    }

    async fn new_session(&self, patp: Arc<str>, id: u64, jh: JoinHandle<()>) {
        let session = Session::new(jh);
        let mut sessions = self.sessions.lock().await;
        sessions.entry(patp).or_default().insert(id, session);
    }

    async fn finish_session(&self, patp: Arc<str>, id: u64, res: Result<()>) -> Result<()> {
        let mut sessions = self.sessions.lock().await;

        sessions
            .get_mut(&patp)
            .and_then(|v| v.get_mut(&id))
            .ok_or_else(|| anyhow::anyhow!("Session does not exist"))?
            .finish_session(res);

        Ok(())
    }

    async fn cleanup_stale_sessions(&self, stale_time: Duration) {
        let mut sessions = self.sessions.lock().await;
        sessions.retain(|_, v| {
            v.retain(|_, v| match &v.result {
                Ok((_, time)) if time.elapsed() >= stale_time => true,
                _ => false,
            });
            !v.is_empty()
        });
    }

    async fn get_session_status(
        &self,
        patp: &str,
        id: u64,
    ) -> Result<Option<core::result::Result<(), String>>> {
        let mut sessions = self.sessions.lock().await;

        Ok(sessions
            .get_mut(patp)
            .and_then(|v| v.get_mut(&id))
            .ok_or_else(|| anyhow::anyhow!("Session does not exist"))?
            .result
            .as_ref()
            .ok()
            .map(|v| v.0.as_ref().map(|_| ()).map_err(|v| v.to_string())))
    }
}
