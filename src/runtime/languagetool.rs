//! Lifecycle, installation and HTTP client for optional LanguageTool support.
//!
//! This module deliberately owns the Java child. The UI only sends commands,
//! so startup, downloads and checks never block GPUI's foreground thread.

use super::Evt;
use crate::proofreading::{
    LanguageToolCoverage, LanguageToolLocalSource, LanguageToolMode, LanguageToolSettings,
    LanguageToolState, LanguageToolStatus, ProofreadingCategory, ProofreadingIssue,
};
use anyhow::{Context as _, Result};
use futures::StreamExt as _;
use serde::Deserialize;
use sha2::{Digest as _, Sha256};
use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc,
};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, Mutex, RwLock};

pub const VERSION: &str = "6.6";
const DOWNLOAD_URL: &str = "https://languagetool.org/download/LanguageTool-6.6.zip";
const DOWNLOAD_SHA256: &str = "53600506b399bb5ffe1e4c8dec794fd378212f14aaf38ccef9b6f89314d11631";
const MARKER: &str = ".aviary-managed";
const SERVER_JAR: &str = "languagetool-server.jar";
const MAX_ARCHIVE_BYTES: u64 = 512 * 1024 * 1024;
const MAX_EXTRACTED_BYTES: u64 = 2 * 1024 * 1024 * 1024;

struct LocalProcess {
    child: Child,
    endpoint: String,
    restarted: bool,
}

#[derive(Default)]
struct RuntimeSession {
    local: Option<LocalProcess>,
}

struct InstallTemps {
    archive: PathBuf,
    staging: PathBuf,
}

impl Drop for InstallTemps {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.archive);
        if self.staging.exists() {
            let _ = std::fs::remove_dir_all(&self.staging);
        }
    }
}

pub(super) struct LanguageToolManager {
    http: reqwest::Client,
    evt_tx: mpsc::UnboundedSender<Evt>,
    config: RwLock<LanguageToolSettings>,
    session: Mutex<RuntimeSession>,
    operation: Mutex<()>,
    ready: AtomicBool,
    /// Incremented before replacing/stopping a server so requests completed by
    /// the previous process cannot overwrite the new session's status.
    session_generation: AtomicU64,
}

impl LanguageToolManager {
    pub fn new(
        http: reqwest::Client,
        evt_tx: mpsc::UnboundedSender<Evt>,
        config: LanguageToolSettings,
    ) -> Arc<Self> {
        Arc::new(Self {
            http,
            evt_tx,
            config: RwLock::new(config),
            session: Mutex::new(RuntimeSession::default()),
            operation: Mutex::new(()),
            ready: AtomicBool::new(false),
            session_generation: AtomicU64::new(0),
        })
    }

    fn emit(&self, event: Evt) {
        let _ = self.evt_tx.send(event);
    }

    fn status(&self, state: LanguageToolState, detail: Option<String>, progress: Option<f32>) {
        self.ready
            .store(state == LanguageToolState::Ready, Ordering::Release);
        let managed_selected = self.config.try_read().is_ok_and(|config| {
            config.mode == LanguageToolMode::LocalManaged
                && config.local_source == LanguageToolLocalSource::Downloaded
        });
        let version = (managed_selected
            && managed_install_dir().is_some_and(|path| path.join(MARKER).is_file()))
        .then(|| VERSION.to_string());
        self.emit(Evt::LanguageToolStatus(LanguageToolStatus {
            state,
            version,
            detail,
            progress,
        }));
    }

    pub async fn configure(&self, config: LanguageToolSettings) {
        let _operation = self.operation.lock().await;
        self.stop().await;
        *self.config.write().await = config.clone();
        match config.mode {
            LanguageToolMode::Disabled => {
                self.status(LanguageToolState::Disabled, None, None);
            }
            LanguageToolMode::ExternalUrl => self.test_current().await,
            LanguageToolMode::LocalManaged => {
                if config.local_source == LanguageToolLocalSource::Downloaded
                    && !managed_install_dir().is_some_and(|path| path.join(MARKER).is_file())
                {
                    self.status(LanguageToolState::NotInstalled, None, None);
                    return;
                }
                self.start_configured_local(&config).await;
            }
        }
    }

    pub async fn stop(&self) {
        self.session_generation.fetch_add(1, Ordering::AcqRel);
        self.ready.store(false, Ordering::Release);
        let child = self
            .session
            .lock()
            .await
            .local
            .take()
            .map(|local| local.child);
        if let Some(mut child) = child {
            let _ = child.start_kill();
            let _ = tokio::time::timeout(std::time::Duration::from_secs(2), child.wait()).await;
        }
    }

    pub async fn reset(&self) {
        let _operation = self.operation.lock().await;
        self.stop().await;
        if let Err(error) = remove_managed_install() {
            log::warn!("LanguageTool factory-reset cleanup: {error:#}");
        }
        *self.config.write().await = LanguageToolSettings::default();
        self.status(LanguageToolState::Disabled, None, None);
    }

    async fn start_configured_local(&self, config: &LanguageToolSettings) {
        self.status(LanguageToolState::Starting, None, None);
        match start_local(&self.http, config).await {
            Ok(local) => {
                self.session.lock().await.local = Some(local);
                self.status(LanguageToolState::Ready, None, None);
            }
            Err(error) => {
                log::warn!("LanguageTool startup: {error:#}");
                self.status(LanguageToolState::Error, Some(format!("{error:#}")), None);
            }
        }
    }

    async fn test_current(&self) {
        let config = self.config.read().await.clone();
        if config.mode == LanguageToolMode::Disabled {
            self.status(LanguageToolState::Disabled, None, None);
            return;
        }
        if config.mode == LanguageToolMode::LocalManaged {
            self.start_configured_local(&config).await;
            return;
        }

        self.status(LanguageToolState::Starting, None, None);
        let result = normalize_base_url(&config.external_url);
        match result {
            Ok(url) => match probe(&self.http, &url).await {
                Ok(()) => self.status(LanguageToolState::Ready, None, None),
                Err(error) => {
                    log::warn!("LanguageTool external probe: {error:#}");
                    self.status(LanguageToolState::Error, Some(format!("{error:#}")), None);
                }
            },
            Err(error) => self.status(LanguageToolState::Error, Some(format!("{error:#}")), None),
        }
    }

    pub async fn test_config(&self, config: LanguageToolSettings) {
        let _operation = self.operation.lock().await;
        self.stop().await;
        *self.config.write().await = config;
        self.test_current().await;
    }

    pub async fn install(&self) {
        let _operation = self.operation.lock().await;
        self.stop().await;
        self.status(LanguageToolState::Installing, None, Some(0.0));
        match self.download_and_install().await {
            Ok(()) => {
                self.status(LanguageToolState::Stopped, None, Some(1.0));
                let config = self.config.read().await.clone();
                if config.mode == LanguageToolMode::LocalManaged
                    && config.local_source == LanguageToolLocalSource::Downloaded
                {
                    self.start_configured_local(&config).await;
                }
            }
            Err(error) => {
                log::warn!("LanguageTool installation: {error:#}");
                self.status(LanguageToolState::Error, Some(format!("{error:#}")), None);
            }
        }
    }

    async fn download_and_install(&self) -> Result<()> {
        let final_dir = managed_install_dir().context("LanguageTool data directory unavailable")?;
        let parent = final_dir
            .parent()
            .context("invalid LanguageTool data directory")?;
        tokio::fs::create_dir_all(parent).await?;
        let nonce = random_suffix();
        let archive_path = parent.join(format!(".download-{nonce}.zip"));
        let staging = parent.join(format!(".stage-{nonce}"));
        let _temps = InstallTemps {
            archive: archive_path.clone(),
            staging: staging.clone(),
        };

        let result = async {
            let response = self
                .http
                .get(DOWNLOAD_URL)
                .send()
                .await
                .context("downloading LanguageTool")?
                .error_for_status()?;
            if response
                .content_length()
                .is_some_and(|size| size > MAX_ARCHIVE_BYTES)
            {
                anyhow::bail!("LanguageTool archive exceeds the size limit");
            }
            let total = response.content_length();
            let mut stream = response.bytes_stream();
            let mut file = tokio::fs::File::create(&archive_path).await?;
            let mut digest = Sha256::new();
            let mut received = 0_u64;
            use tokio::io::AsyncWriteExt as _;
            while let Some(chunk) = stream.next().await {
                let chunk = chunk.context("reading LanguageTool download")?;
                received = received.saturating_add(chunk.len() as u64);
                if received > MAX_ARCHIVE_BYTES {
                    anyhow::bail!("LanguageTool archive exceeds the size limit");
                }
                digest.update(&chunk);
                file.write_all(&chunk).await?;
                self.status(
                    LanguageToolState::Installing,
                    None,
                    total.map(|total| (received as f32 / total.max(1) as f32).clamp(0.0, 1.0)),
                );
            }
            file.flush().await?;
            drop(file);
            let actual = hex::encode(digest.finalize());
            verify_download_digest(&actual)?;

            let archive = archive_path.clone();
            let stage = staging.clone();
            tokio::task::spawn_blocking(move || extract_archive(&archive, &stage))
                .await
                .context("LanguageTool extraction task")??;
            if !staging.join(SERVER_JAR).is_file() {
                anyhow::bail!("LanguageTool archive does not contain {SERVER_JAR}");
            }
            tokio::fs::write(staging.join(MARKER), format!("LanguageTool {VERSION}\n")).await?;
            let stage = staging.clone();
            let destination = final_dir.clone();
            tokio::task::spawn_blocking(move || replace_managed_install(&stage, &destination))
                .await
                .context("LanguageTool install task")??;
            Result::<()>::Ok(())
        }
        .await;

        result
    }

    pub async fn uninstall(&self) {
        let _operation = self.operation.lock().await;
        self.stop().await;
        match tokio::task::spawn_blocking(remove_managed_install).await {
            Ok(Ok(())) => self.status(LanguageToolState::NotInstalled, None, None),
            Ok(Err(error)) => {
                log::warn!("LanguageTool uninstall: {error:#}");
                self.status(LanguageToolState::Error, Some(format!("{error:#}")), None);
            }
            Err(error) => {
                self.status(LanguageToolState::Error, Some(error.to_string()), None);
            }
        }
    }

    pub async fn check(
        &self,
        editor_id: String,
        block_id: u64,
        revision: u64,
        text: String,
        ui_language: String,
    ) {
        let session_generation = self.session_generation.load(Ordering::Acquire);
        let config = self.config.read().await.clone();
        if config.mode == LanguageToolMode::Disabled {
            return;
        }
        if !self.ready.load(Ordering::Acquire) {
            self.emit(Evt::LanguageToolCheckFailed {
                editor_id,
                block_id,
                revision,
                error: "LanguageTool is not ready".to_string(),
            });
            return;
        }
        if text.trim().is_empty() {
            self.emit(Evt::LanguageToolChecked {
                editor_id,
                block_id,
                revision,
                source: text,
                issues: Vec::new(),
            });
            return;
        }
        let endpoint_result = {
            let _operation = self.operation.lock().await;
            if !self
                .check_context_is_current(session_generation, &config)
                .await
            {
                self.cancel_check(editor_id, block_id, revision);
                return;
            }
            self.endpoint(&config).await
        };
        let endpoint = match endpoint_result {
            Ok(endpoint) => endpoint,
            Err(error) => {
                let _operation = self.operation.lock().await;
                if !self
                    .check_context_is_current(session_generation, &config)
                    .await
                {
                    self.cancel_check(editor_id, block_id, revision);
                    return;
                }
                log::warn!("LanguageTool unavailable: {error:#}");
                self.status(LanguageToolState::Error, Some(format!("{error:#}")), None);
                self.emit(Evt::LanguageToolCheckFailed {
                    editor_id,
                    block_id,
                    revision,
                    error: format!("{error:#}"),
                });
                return;
            }
        };
        let result = check_text(&self.http, &endpoint, &text, config.coverage, &ui_language).await;
        let _operation = self.operation.lock().await;
        if !self
            .check_context_is_current(session_generation, &config)
            .await
        {
            self.cancel_check(editor_id, block_id, revision);
            return;
        }
        match result {
            Ok(issues) => {
                self.status(LanguageToolState::Ready, None, None);
                self.emit(Evt::LanguageToolChecked {
                    editor_id,
                    block_id,
                    revision,
                    source: text,
                    issues,
                });
            }
            Err(error) => {
                log::warn!("LanguageTool check: {error:#}");
                self.status(LanguageToolState::Error, Some(format!("{error:#}")), None);
                self.emit(Evt::LanguageToolCheckFailed {
                    editor_id,
                    block_id,
                    revision,
                    error: format!("{error:#}"),
                });
            }
        }
    }

    async fn check_context_is_current(
        &self,
        session_generation: u64,
        config: &LanguageToolSettings,
    ) -> bool {
        let same_config = *self.config.read().await == *config;
        same_config && self.session_generation.load(Ordering::Acquire) == session_generation
    }

    fn cancel_check(&self, editor_id: String, block_id: u64, revision: u64) {
        self.emit(Evt::LanguageToolCheckFailed {
            editor_id,
            block_id,
            revision,
            error: "LanguageTool session changed during the check".to_string(),
        });
    }

    async fn endpoint(&self, config: &LanguageToolSettings) -> Result<String> {
        if config.mode == LanguageToolMode::ExternalUrl {
            return normalize_base_url(&config.external_url);
        }
        let mut session = self.session.lock().await;
        if let Some(local) = &mut session.local {
            match local.child.try_wait() {
                Ok(None) => return Ok(local.endpoint.clone()),
                Ok(Some(status)) if !local.restarted => {
                    log::warn!("LanguageTool exited unexpectedly with {status}; restarting once");
                }
                Ok(Some(status)) => anyhow::bail!("LanguageTool exited unexpectedly with {status}"),
                Err(error) => return Err(error.into()),
            }
        }
        let should_restart = session.local.as_ref().is_some_and(|local| !local.restarted);
        session.local.take();
        drop(session);
        let mut local = start_local(&self.http, config).await?;
        local.restarted = should_restart;
        let endpoint = local.endpoint.clone();
        self.session.lock().await.local = Some(local);
        Ok(endpoint)
    }
}

fn managed_install_dir() -> Option<PathBuf> {
    directories::ProjectDirs::from("be", "acetics", "aviary").map(|dirs| {
        dirs.data_dir()
            .join("languagetool")
            .join(format!("LanguageTool-{VERSION}"))
    })
}

fn random_suffix() -> String {
    let mut bytes = [0_u8; 8];
    if getrandom::fill(&mut bytes).is_err() {
        return format!("{}", std::process::id());
    }
    hex::encode(bytes)
}

fn remove_managed_install() -> Result<()> {
    let Some(path) = managed_install_dir() else {
        return Ok(());
    };
    remove_managed_install_at(&path)
}

fn remove_managed_install_at(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !path.join(MARKER).is_file() {
        anyhow::bail!("refusing to remove a LanguageTool directory not managed by Aviary");
    }
    std::fs::remove_dir_all(path)?;
    Ok(())
}

fn verify_download_digest(actual: &str) -> Result<()> {
    if actual.eq_ignore_ascii_case(DOWNLOAD_SHA256) {
        Ok(())
    } else {
        anyhow::bail!("LanguageTool SHA-256 mismatch (expected {DOWNLOAD_SHA256}, got {actual})")
    }
}

fn replace_managed_install(staging: &Path, destination: &Path) -> Result<()> {
    if !staging.join(MARKER).is_file() {
        anyhow::bail!("refusing an unmarked LanguageTool staging directory");
    }
    if !destination.exists() {
        std::fs::rename(staging, destination)?;
        return Ok(());
    }
    if std::fs::symlink_metadata(destination)?
        .file_type()
        .is_symlink()
        || !destination.join(MARKER).is_file()
    {
        anyhow::bail!("refusing to replace a LanguageTool directory not managed by Aviary");
    }
    let backup = destination.with_file_name(format!(".backup-{}", random_suffix()));
    std::fs::rename(destination, &backup)?;
    if let Err(error) = std::fs::rename(staging, destination) {
        let _ = std::fs::rename(&backup, destination);
        return Err(error.into());
    }
    if let Err(error) = std::fs::remove_dir_all(&backup) {
        log::warn!("failed to remove old LanguageTool installation: {error}");
    }
    Ok(())
}

fn extract_archive(archive_path: &Path, destination: &Path) -> Result<()> {
    std::fs::create_dir_all(destination)?;
    let file = std::fs::File::open(archive_path)?;
    let mut archive = zip::ZipArchive::new(file).context("opening LanguageTool ZIP")?;
    let mut entries = Vec::with_capacity(archive.len());
    let mut extracted_size = 0_u64;
    for index in 0..archive.len() {
        let entry = archive.by_index(index)?;
        let raw = entry.name();
        if raw.contains('\\') || raw.as_bytes().contains(&0) {
            anyhow::bail!("unsafe path in LanguageTool ZIP: {raw:?}");
        }
        let path = entry
            .enclosed_name()
            .context("path escapes the LanguageTool destination")?;
        if path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        }) {
            anyhow::bail!("path escapes the LanguageTool destination");
        }
        if entry
            .unix_mode()
            .is_some_and(|mode| mode & 0o170000 == 0o120000)
        {
            anyhow::bail!("symbolic links are not allowed in the LanguageTool ZIP");
        }
        extracted_size = extracted_size.saturating_add(entry.size());
        if extracted_size > MAX_EXTRACTED_BYTES {
            anyhow::bail!("LanguageTool extraction exceeds the size limit");
        }
        entries.push((path, entry.is_dir()));
    }
    // Official LanguageTool archives include both `LanguageTool-6.6/` and
    // entries below it. The root directory entry must not prevent stripping
    // that single common directory from the extracted distribution.
    let common_root = entries
        .iter()
        .find(|(path, _)| path.components().count() > 1)
        .and_then(|(path, _)| path.components().next())
        .map(|component| component.as_os_str().to_owned())
        .filter(|root| {
            entries.iter().all(|(path, is_dir)| {
                path.components()
                    .next()
                    .is_some_and(|component| component.as_os_str() == root)
                    && (path.components().count() > 1 || *is_dir)
            })
        });

    for (index, (stored_path, _)) in entries.into_iter().enumerate() {
        let relative = if common_root.is_some() {
            stored_path.components().skip(1).collect::<PathBuf>()
        } else {
            stored_path
        };
        if relative.as_os_str().is_empty() {
            continue;
        }
        let output = destination.join(relative);
        if !output.starts_with(destination) {
            anyhow::bail!("path escapes the LanguageTool destination");
        }
        let mut entry = archive.by_index(index)?;
        if entry.is_dir() {
            std::fs::create_dir_all(&output)?;
            continue;
        }
        if let Some(parent) = output.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut file = std::fs::File::create(&output)?;
        std::io::copy(&mut entry, &mut file)?;
    }
    Ok(())
}

fn distribution_dir(config: &LanguageToolSettings) -> Result<PathBuf> {
    let requested = match config.local_source {
        LanguageToolLocalSource::Downloaded => {
            managed_install_dir().context("LanguageTool data directory unavailable")?
        }
        LanguageToolLocalSource::ExistingDirectory => {
            let value = config.existing_directory.trim();
            if value.is_empty() {
                anyhow::bail!("no existing LanguageTool directory configured");
            }
            PathBuf::from(value)
        }
    };
    if requested.join(SERVER_JAR).is_file() {
        return Ok(requested);
    }
    let children = std::fs::read_dir(&requested)
        .with_context(|| format!("reading {}", requested.display()))?;
    for child in children.flatten() {
        if child.path().join(SERVER_JAR).is_file() {
            return Ok(child.path());
        }
    }
    anyhow::bail!("{} does not contain {SERVER_JAR}", requested.display())
}

async fn start_local(
    http: &reqwest::Client,
    config: &LanguageToolSettings,
) -> Result<LocalProcess> {
    let config = config.clone();
    let (java, directory) = tokio::task::spawn_blocking(move || {
        Ok::<_, anyhow::Error>((detect_java(&config.java_path)?, distribution_dir(&config)?))
    })
    .await??;
    let mut last_error = None;
    for _ in 0..3 {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
        let port = listener.local_addr()?.port();
        drop(listener);
        let mut command = Command::new(&java);
        command
            .current_dir(&directory)
            .arg("-Xms32m")
            .arg("-Xmx512m")
            .arg("-cp")
            .arg(SERVER_JAR)
            .arg("org.languagetool.server.HTTPServer")
            .arg("--port")
            .arg(port.to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let mut child = command
            .spawn()
            .with_context(|| format!("starting {}", java.display()))?;
        let endpoint = format!("http://127.0.0.1:{port}/v2");
        match wait_until_ready(http, &endpoint, &mut child).await {
            Ok(()) => {
                return Ok(LocalProcess {
                    child,
                    endpoint,
                    restarted: false,
                })
            }
            Err(error) => {
                let _ = child.start_kill();
                let _ = child.wait().await;
                last_error = Some(error);
            }
        }
    }
    Err(last_error.unwrap_or_else(|| anyhow::anyhow!("LanguageTool did not start")))
}

async fn wait_until_ready(http: &reqwest::Client, endpoint: &str, child: &mut Child) -> Result<()> {
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(8);
    loop {
        if let Some(status) = child.try_wait()? {
            anyhow::bail!("LanguageTool exited during startup with {status}");
        }
        if probe(http, endpoint).await.is_ok() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            anyhow::bail!("LanguageTool startup timed out");
        }
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

fn normalize_base_url(value: &str) -> Result<String> {
    let mut url = reqwest::Url::parse(value.trim()).context("invalid LanguageTool URL")?;
    if !matches!(url.scheme(), "http" | "https") {
        anyhow::bail!("LanguageTool URL must use HTTP or HTTPS");
    }
    url.set_query(None);
    url.set_fragment(None);
    let path = url.path().trim_end_matches('/');
    let path = if path.ends_with("/v2") || path == "/v2" {
        path.to_string()
    } else if path.is_empty() || path == "/" {
        "/v2".to_string()
    } else {
        format!("{path}/v2")
    };
    url.set_path(&path);
    Ok(url.as_str().trim_end_matches('/').to_string())
}

async fn probe(http: &reqwest::Client, endpoint: &str) -> Result<()> {
    http.get(format!("{endpoint}/languages"))
        .timeout(std::time::Duration::from_secs(4))
        .send()
        .await?
        .error_for_status()?;
    Ok(())
}

#[derive(Deserialize)]
struct ApiResponse {
    #[serde(default)]
    language: Option<ApiLanguage>,
    matches: Vec<ApiMatch>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiLanguage {
    code: String,
    #[serde(default)]
    detected_language: Option<ApiDetectedLanguage>,
}

#[derive(Deserialize)]
struct ApiDetectedLanguage {
    code: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiMatch {
    message: String,
    offset: usize,
    length: usize,
    replacements: Vec<ApiReplacement>,
    rule: ApiRule,
}

#[derive(Deserialize)]
struct ApiReplacement {
    value: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ApiRule {
    id: String,
    #[serde(default)]
    issue_type: String,
    category: ApiCategory,
}

#[derive(Deserialize)]
struct ApiCategory {
    id: String,
}

async fn check_text(
    http: &reqwest::Client,
    endpoint: &str,
    text: &str,
    coverage: LanguageToolCoverage,
    ui_language: &str,
) -> Result<Vec<ProofreadingIssue>> {
    let mut response = request_check(http, endpoint, text, "auto").await?;
    let detected_language = response.language.as_ref().map(|language| {
        language
            .detected_language
            .as_ref()
            .map_or(language.code.as_str(), |detected| detected.code.as_str())
    });
    if let Some(fallback) = short_text_language_fallback(text, detected_language, ui_language) {
        response = request_check(http, endpoint, text, fallback).await?;
    }
    decode_matches(text, response.matches, coverage)
}

async fn request_check(
    http: &reqwest::Client,
    endpoint: &str,
    text: &str,
    language: &str,
) -> Result<ApiResponse> {
    let mut form = vec![("text", text), ("language", language)];
    if language == "auto" {
        form.push(("preferredVariants", "en-US"));
    }
    Ok(http
        .post(format!("{endpoint}/check"))
        .timeout(std::time::Duration::from_secs(12))
        .form(&form)
        .send()
        .await?
        .error_for_status()?
        .json::<ApiResponse>()
        .await?)
}

fn short_text_language_fallback(
    text: &str,
    detected_language: Option<&str>,
    ui_language: &str,
) -> Option<&'static str> {
    if text.chars().count() > 50 {
        return None;
    }
    let detected = detected_language?
        .split(['-', '_'])
        .next()
        .unwrap_or_default();
    if matches!(detected, "fr" | "en") {
        return None;
    }
    Some(if ui_language.starts_with("fr") {
        "fr"
    } else {
        "en-US"
    })
}

fn decode_matches(
    text: &str,
    matches: Vec<ApiMatch>,
    coverage: LanguageToolCoverage,
) -> Result<Vec<ProofreadingIssue>> {
    matches
        .into_iter()
        .filter_map(|matched| {
            let category = classify(&matched.rule);
            if coverage == LanguageToolCoverage::GrammarOnly
                && category == ProofreadingCategory::Spelling
            {
                return None;
            }
            Some((matched, category))
        })
        .map(|(matched, category)| {
            let range =
                utf16_range_to_utf8(text, matched.offset, matched.length).with_context(|| {
                    format!(
                        "invalid LanguageTool offset {}+{}",
                        matched.offset, matched.length
                    )
                })?;
            Ok(ProofreadingIssue {
                range,
                category,
                message: matched.message,
                rule_id: matched.rule.id,
                replacements: matched
                    .replacements
                    .into_iter()
                    .take(8)
                    .map(|replacement| replacement.value)
                    .collect(),
            })
        })
        .collect()
}

fn classify(rule: &ApiRule) -> ProofreadingCategory {
    let issue = rule.issue_type.to_ascii_lowercase();
    let category = rule.category.id.to_ascii_uppercase();
    if issue.contains("misspell") || matches!(category.as_str(), "TYPOS" | "SPELLING") {
        ProofreadingCategory::Spelling
    } else if issue.contains("typograph")
        || category.contains("TYPOGRAPH")
        || category == "PUNCTUATION"
    {
        ProofreadingCategory::Typography
    } else if issue.contains("style") || category.contains("STYLE") {
        ProofreadingCategory::Style
    } else {
        ProofreadingCategory::Grammar
    }
}

/// LanguageTool reports Java/UTF-16 code-unit offsets. Rust inputs use UTF-8
/// byte offsets, so reject offsets in the middle of a surrogate pair and map
/// valid boundaries exactly (combining marks remain independent boundaries).
fn utf16_range_to_utf8(text: &str, offset: usize, length: usize) -> Option<std::ops::Range<usize>> {
    fn boundary(text: &str, target: usize) -> Option<usize> {
        let mut units = 0;
        for (byte, character) in text.char_indices() {
            if units == target {
                return Some(byte);
            }
            units += character.len_utf16();
            if units > target {
                return None;
            }
        }
        (units == target).then_some(text.len())
    }
    let end = offset.checked_add(length)?;
    Some(boundary(text, offset)?..boundary(text, end)?)
}

fn java_executable_in(path: &Path) -> Vec<PathBuf> {
    let name = if cfg!(windows) { "java.exe" } else { "java" };
    if path.is_file() {
        return vec![path.to_path_buf()];
    }
    vec![path.join("bin").join(name), path.join(name)]
}

fn java_candidates(configured: &str) -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if !configured.trim().is_empty() {
        candidates.extend(java_executable_in(Path::new(configured.trim())));
        return candidates;
    }
    if let Some(home) = std::env::var_os("JAVA_HOME") {
        candidates.extend(java_executable_in(Path::new(&home)));
    }
    if let Some(path) = std::env::var_os("PATH") {
        let name = if cfg!(windows) { "java.exe" } else { "java" };
        candidates.extend(std::env::split_paths(&path).map(|directory| directory.join(name)));
    }
    #[cfg(target_os = "linux")]
    {
        candidates.extend([
            PathBuf::from("/usr/bin/java"),
            PathBuf::from("/usr/local/bin/java"),
        ]);
        if let Ok(homes) = std::fs::read_dir("/usr/lib/jvm") {
            candidates.extend(homes.flatten().map(|home| home.path().join("bin/java")));
        }
    }
    #[cfg(target_os = "macos")]
    {
        candidates.extend([
            PathBuf::from("/usr/bin/java"),
            PathBuf::from("/opt/homebrew/opt/openjdk/bin/java"),
            PathBuf::from("/usr/local/opt/openjdk/bin/java"),
        ]);
        if let Ok(homes) = std::fs::read_dir("/Library/Java/JavaVirtualMachines") {
            candidates.extend(
                homes
                    .flatten()
                    .map(|home| home.path().join("Contents/Home/bin/java")),
            );
        }
    }
    #[cfg(target_os = "windows")]
    for variable in ["ProgramFiles", "ProgramFiles(x86)"] {
        if let Some(root) = std::env::var_os(variable) {
            for vendor in ["Java", "Eclipse Adoptium", "Microsoft"] {
                if let Ok(homes) = std::fs::read_dir(Path::new(&root).join(vendor)) {
                    candidates.extend(
                        homes
                            .flatten()
                            .map(|home| home.path().join("bin").join("java.exe")),
                    );
                }
            }
        }
    }
    let mut seen = HashSet::new();
    candidates.retain(|path| seen.insert(path.clone()));
    candidates
}

fn detect_java(configured: &str) -> Result<PathBuf> {
    let mut errors = Vec::new();
    for candidate in java_candidates(configured) {
        if !candidate.is_file() {
            continue;
        }
        match validate_java(&candidate) {
            Ok(major) if major >= 17 => return Ok(candidate),
            Ok(major) => errors.push(format!(
                "{} is Java {major}; Java 17+ is required",
                candidate.display()
            )),
            Err(error) => errors.push(format!("{}: {error:#}", candidate.display())),
        }
    }
    if errors.is_empty() {
        anyhow::bail!("Java 17 or newer was not found");
    }
    anyhow::bail!(errors.join("; "))
}

fn validate_java(path: &Path) -> Result<u32> {
    let output = std::process::Command::new(path)
        .arg("-version")
        .output()
        .with_context(|| format!("running {} -version", path.display()))?;
    let version_text = format!(
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    if !output.status.success() {
        anyhow::bail!("java -version failed: {}", version_text.trim());
    }
    parse_java_major(&version_text).context("unrecognized java -version output")
}

fn parse_java_major(output: &str) -> Option<u32> {
    let version = output.split('"').nth(1).or_else(|| {
        output
            .split_whitespace()
            .find(|word| word.chars().next().is_some_and(|c| c.is_ascii_digit()))
    })?;
    let mut parts = version.split(['.', '-', '_']);
    let first = parts.next()?.parse::<u32>().ok()?;
    if first == 1 {
        parts.next()?.parse().ok()
    } else {
        Some(first)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_supported_and_legacy_java_versions() {
        assert_eq!(
            parse_java_major("openjdk version \"21.0.2\" 2024-01-16"),
            Some(21)
        );
        assert_eq!(parse_java_major("java version \"1.8.0_402\""), Some(8));
    }

    #[test]
    fn converts_utf16_offsets_for_accents_combining_marks_and_emoji() {
        let text = "é e\u{301} 😀 fin";
        assert_eq!(utf16_range_to_utf8(text, 0, 1), Some(0..2));
        assert_eq!(utf16_range_to_utf8(text, 2, 2), Some(3..6));
        assert_eq!(utf16_range_to_utf8(text, 5, 2), Some(7..11));
        assert_eq!(utf16_range_to_utf8(text, 6, 1), None);
    }

    #[test]
    fn normalizes_external_api_urls() {
        assert_eq!(
            normalize_base_url("https://example.test/").unwrap(),
            "https://example.test/v2"
        );
        assert_eq!(
            normalize_base_url("http://localhost:8081/v2/").unwrap(),
            "http://localhost:8081/v2"
        );
        assert!(normalize_base_url("file:///tmp/tool").is_err());
    }

    #[test]
    fn grammar_only_filters_spelling_matches() {
        let matches = vec![ApiMatch {
            message: "Typo".into(),
            offset: 0,
            length: 3,
            replacements: vec![],
            rule: ApiRule {
                id: "MORFOLOGIK_RULE_EN_US".into(),
                issue_type: "misspelling".into(),
                category: ApiCategory { id: "TYPOS".into() },
            },
        }];
        assert!(
            decode_matches("bad", matches, LanguageToolCoverage::GrammarOnly)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn short_foreign_detection_falls_back_to_the_ui_language() {
        assert_eq!(
            short_text_language_fallback("sur cela", Some("eo"), "fr-FR"),
            Some("fr")
        );
        assert_eq!(
            short_text_language_fallback("about this", Some("eo"), "en"),
            Some("en-US")
        );
        assert_eq!(
            short_text_language_fallback("about this", Some("en-US"), "fr"),
            None
        );
        assert_eq!(
            short_text_language_fallback(&"vorto ".repeat(10), Some("eo"), "fr"),
            None
        );
    }

    #[test]
    fn rejects_hostile_zip_paths() {
        let root = std::env::temp_dir().join(format!("aviary-lt-test-{}", random_suffix()));
        std::fs::create_dir_all(&root).unwrap();
        let archive_path = root.join("hostile.zip");
        let file = std::fs::File::create(&archive_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        writer
            .start_file("../outside", zip::write::SimpleFileOptions::default())
            .unwrap();
        use std::io::Write as _;
        writer.write_all(b"bad").unwrap();
        writer.finish().unwrap();
        assert!(extract_archive(&archive_path, &root.join("out")).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn strips_the_official_archive_root_directory_entry() {
        let root = std::env::temp_dir().join(format!("aviary-lt-root-{}", random_suffix()));
        std::fs::create_dir_all(&root).unwrap();
        let archive_path = root.join("LanguageTool-6.6.zip");
        let file = std::fs::File::create(&archive_path).unwrap();
        let mut writer = zip::ZipWriter::new(file);
        let options = zip::write::SimpleFileOptions::default();
        writer.add_directory("LanguageTool-6.6/", options).unwrap();
        writer
            .start_file("LanguageTool-6.6/languagetool-server.jar", options)
            .unwrap();
        use std::io::Write as _;
        writer.write_all(b"jar").unwrap();
        writer
            .start_file("LanguageTool-6.6/libs/dependency.jar", options)
            .unwrap();
        writer.write_all(b"dependency").unwrap();
        writer.finish().unwrap();

        let destination = root.join("out");
        extract_archive(&archive_path, &destination).unwrap();
        assert!(destination.join(SERVER_JAR).is_file());
        assert!(destination.join("libs/dependency.jar").is_file());
        assert!(!destination.join("LanguageTool-6.6").exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn rejects_incorrect_download_digest() {
        assert!(verify_download_digest(&"0".repeat(64)).is_err());
        assert!(verify_download_digest(DOWNLOAD_SHA256).is_ok());
    }

    #[test]
    fn interrupted_install_cleans_temporary_paths() {
        let root = std::env::temp_dir().join(format!("aviary-lt-interrupted-{}", random_suffix()));
        let archive = root.join("partial.zip");
        let staging = root.join("partial-stage");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(&archive, b"partial").unwrap();
        {
            let _temps = InstallTemps {
                archive: archive.clone(),
                staging: staging.clone(),
            };
        }
        assert!(!archive.exists());
        assert!(!staging.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn atomic_install_replaces_only_a_marked_destination() {
        let root = std::env::temp_dir().join(format!("aviary-lt-atomic-{}", random_suffix()));
        let destination = root.join("LanguageTool-6.6");
        let staging = root.join("stage");
        std::fs::create_dir_all(&destination).unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(destination.join(MARKER), b"old").unwrap();
        std::fs::write(destination.join("old"), b"old").unwrap();
        std::fs::write(staging.join(MARKER), b"new").unwrap();
        std::fs::write(staging.join("new"), b"new").unwrap();
        replace_managed_install(&staging, &destination).unwrap();
        assert!(destination.join("new").is_file());
        assert!(!destination.join("old").exists());

        let unowned = root.join("unowned");
        let second_stage = root.join("second-stage");
        std::fs::create_dir_all(&unowned).unwrap();
        std::fs::create_dir_all(&second_stage).unwrap();
        std::fs::write(second_stage.join(MARKER), b"new").unwrap();
        assert!(replace_managed_install(&second_stage, &unowned).is_err());
        assert!(unowned.exists());
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn uninstall_refuses_unmarked_directories() {
        let root = std::env::temp_dir().join(format!("aviary-lt-remove-{}", random_suffix()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(root.join("user-file"), b"keep").unwrap();
        assert!(remove_managed_install_at(&root).is_err());
        assert!(root.join("user-file").is_file());
        std::fs::write(root.join(MARKER), b"managed").unwrap();
        remove_managed_install_at(&root).unwrap();
        assert!(!root.exists());
    }

    #[cfg(unix)]
    #[test]
    fn validates_java_paths_with_spaces_and_rejects_old_versions() {
        use std::os::unix::fs::PermissionsExt as _;
        let root = std::env::temp_dir().join(format!("aviary java {}", random_suffix()));
        std::fs::create_dir_all(&root).unwrap();
        let java = root.join("java executable");
        std::fs::write(&java, "#!/bin/sh\necho 'openjdk version \"17.0.12\"' >&2\n").unwrap();
        let mut permissions = std::fs::metadata(&java).unwrap().permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&java, permissions.clone()).unwrap();
        assert_eq!(validate_java(&java).unwrap(), 17);
        assert_eq!(detect_java(java.to_str().unwrap()).unwrap(), java);
        std::fs::write(&java, "#!/bin/sh\necho 'java version \"1.8.0_402\"' >&2\n").unwrap();
        std::fs::set_permissions(&java, permissions).unwrap();
        assert_eq!(validate_java(&java).unwrap(), 8);
        assert!(detect_java(java.to_str().unwrap()).is_err());
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn decodes_checks_from_an_external_http_server() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let count = stream.read(&mut request).await.unwrap();
            let request = String::from_utf8_lossy(&request[..count]);
            assert!(request.starts_with("POST /v2/check"));
            let body = r#"{"language":{"name":"French","code":"fr","detectedLanguage":{"name":"French","code":"fr"}},"matches":[{"message":"Accord","offset":3,"length":2,"replacements":[{"value":"va"}],"rule":{"id":"RULE","issueType":"grammar","category":{"id":"GRAMMAR"}}}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(), body
            );
            stream.write_all(response.as_bytes()).await.unwrap();
        });
        let text = "😀 e\u{301} va";
        let issues = check_text(
            &reqwest::Client::new(),
            &format!("http://{address}/v2"),
            text,
            LanguageToolCoverage::SpellingAndGrammar,
            "fr",
        )
        .await
        .unwrap();
        server.await.unwrap();
        assert_eq!(issues.len(), 1);
        assert_eq!(&text[issues[0].range.clone()], "e\u{301}");
        assert_eq!(issues[0].replacements, ["va"]);
    }

    #[tokio::test]
    async fn an_old_connection_error_does_not_replace_the_new_session_status() {
        use tokio::io::AsyncReadExt as _;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (accepted_tx, accepted_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut request = vec![0_u8; 4096];
            let _ = stream.read(&mut request).await.unwrap();
            let _ = accepted_tx.send(());
            let _ = release_rx.await;
            // Dropping the stream simulates Java being stopped by the
            // connection test before its response has completed.
        });

        let (evt_tx, mut evt_rx) = mpsc::unbounded_channel();
        let config = LanguageToolSettings {
            mode: LanguageToolMode::ExternalUrl,
            external_url: format!("http://{address}"),
            ..LanguageToolSettings::default()
        };
        let manager = LanguageToolManager::new(reqwest::Client::new(), evt_tx, config);
        manager.status(LanguageToolState::Ready, None, None);

        let checking = {
            let manager = manager.clone();
            tokio::spawn(async move {
                manager
                    .check("editor".into(), 1, 1, "Une phrase.".into(), "fr".into())
                    .await;
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(2), accepted_rx)
            .await
            .unwrap()
            .unwrap();
        manager.stop().await;
        let _ = release_tx.send(());
        checking.await.unwrap();
        server.await.unwrap();

        let events: Vec<_> = std::iter::from_fn(|| evt_rx.try_recv().ok()).collect();
        assert!(events
            .iter()
            .any(|event| matches!(event, Evt::LanguageToolCheckFailed { .. })));
        assert!(!events.iter().any(|event| matches!(
            event,
            Evt::LanguageToolStatus(LanguageToolStatus {
                state: LanguageToolState::Error,
                ..
            })
        )));
    }
}
