use std::{collections::HashMap, sync::Arc};

use anyhow::Context;
use futures::{TryStreamExt, StreamExt};
use reqwest::Client;
use sled::Db;
use tokio::sync::broadcast::Receiver;

#[derive(Debug, serde::Deserialize)]
struct GithubReleaseResponse {
    id: u64,
    assets: Vec<GithubAsset>,
}

#[derive(Debug, serde::Deserialize)]
struct GithubAsset {
    name: String,
    browser_download_url: String,
}

#[derive(Debug, serde::Deserialize)]
struct Config {
    options: ConfigOptions,
    #[serde(rename = "repo")]
    repos: HashMap<String, RepoConfig>,
}

#[derive(Debug, serde::Deserialize)]
struct ConfigOptions {
    timeout: u64,
    interval: u64,
}

#[derive(Debug, serde::Deserialize)]
struct RepoConfig {
    repo: Either<String, Repo>,
    #[serde(alias = "out-dir")]
    out_dir: String,
    #[serde(alias = "pre-download-command")]
    pre_download_command: Option<Either<String, Vec<String>>>,
    #[serde(alias = "post-download-command")]
    post_download_command: Option<Either<String, Vec<String>>>,
    #[serde(alias = "asset-filter")]
    asset_filter: Option<Vec<String>>,
    interval: Option<u64>,
}

impl RepoConfig {
    #[tracing::instrument(skip_all, fields(name = %name))]
    async fn watch(self, name: String, timeout: u64, mut shutdown: Receiver<()>, client: Client, db: Arc<Db>) -> anyhow::Result<()> {
        let (user, repo_name) = match &self.repo {
            Either::Left(s) => s.split_once('/').with_context(|| format!("invalid repo string \"{}\"", s))?,
            Either::Right(repo) => (&*repo.user, &*repo.name),
        };
        let api_url = format!("https://api.github.com/repos/{}/{}/releases/latest", user, repo_name);
        let mut timer = tokio::time::interval(std::time::Duration::from_secs(
            self.interval.context("[BUG] default interval not set")?,
        ));

        loop {
            tokio::select! {
                _ = timer.tick() => {
                    tracing::trace!("Checking release");
                    let release: GithubReleaseResponse = client
                        .get(&*api_url)
                        .header("User-Agent", concat!("gh-release-watcher v", stringify!(env!("CARGO_PKG_VERSION"))))
                        .timeout(std::time::Duration::from_secs(timeout))
                        .send()
                        .await
                        .context("error retrieving latest release")?
                        .json()
                        .await
                        .context("error deserializing Github release API response")?;

                    let path = std::path::PathBuf::from(&self.out_dir);
                    let previous_files = match db.get(&*name).context("failed to retrieve repo history from database")? {
                        Some(bytes) => {
                            let s = core::str::from_utf8(&bytes).context("invalid UTF-8 was inserted into the database")?;
                            let (id, filename) = s.split_once('|').context("invalid contents inserted into the database")?;
                            let id: u64 = id.parse().context("invalid ID inserted into the database")?;

                            // If this is the same release as last time, wait until the next interval
                            if id == release.id {
                                tracing::trace!("Not a new release, waiting for next tick");
                                continue;
                            }

                            
                            Some(filename.split(',').map(|filename| path.join(filename).display().to_string()).collect::<Vec<_>>())
                        }
                        None => None,
                    };

                    if let Some((previous_files, command)) = previous_files.as_ref().zip(self.pre_download_command.clone()) {
                        let mut pieces = match command {
                            Either::Left(s) => shlex::split(&s).context("invalid pre-download command")?,
                            Either::Right(pieces) => pieces,
                        };

                        let replace_indices: Vec<_> = pieces.iter_mut().enumerate().filter_map(|(i, s)| match s == "{}" { true => Some(i), false => None }).collect();

                        for file in previous_files {
                            for index in &replace_indices {
                                pieces[*index] = file.clone();
                            }

                            let mut command = tokio::process::Command::new(pieces.get(0).context("empty pre-download command")?);
                            command.args(&pieces[1..]);

                            let mut child = command.spawn().context("failed to spawn pre-download process")?;
                            let exit_status = child.wait().await.context("failed to wait on child process")?;
                            if !exit_status.success() {
                                return Err(anyhow::anyhow!("pre-download command failed to execute successfully: {:?} (exit code: {})", pieces, exit_status));
                            }
                        }
                    }

                    if let Some(previous_files) = previous_files {
                        for previous_file in previous_files {
                            tokio::fs::remove_file(previous_file).await.context("failed to remove previously downloaded file")?;
                        }
                    }
                    let download_results = futures::stream::iter(&release.assets).filter(|asset| async {
                        match &self.asset_filter {
                            Some(filter) => filter.contains(&asset.name),
                            None => true,
                        }
                    }).map::<anyhow::Result<_>, _>(Ok).and_then(|asset| {
                        let client = client.clone();
                        let path = path.join(&asset.name);
                        async move {
                            let mut file = tokio::fs::File::create(&path).await.context("failed to create new file on disk")?;
                            let response = client.get(&asset.browser_download_url).send().await.context("failed to download file from Github")?;
                            let bytes = response.bytes().await.context("failed to get bytes from file download response")?;
                            tokio::io::copy(&mut &*bytes, &mut file).await.context("failed to write contents to file")?;

                            Ok(path)
                        }
                    }).collect::<Vec<_>>().await;

                    if download_results.is_empty() {
                        continue;
                    }

                    let mut pieces = match self.post_download_command.clone() {
                        Some(command) => Some(match command {
                            Either::Left(s) => shlex::split(&s).context("invalid post-download command")?,
                            Either::Right(pieces) => pieces,
                        }),
                        None => None
                    };

                    let replace_indices: Vec<_> = pieces.as_mut().map(|pieces| pieces.iter_mut().enumerate().filter_map(|(i, s)| match s == "{}" { true => Some(i), false => None }).collect()).unwrap_or_default();

                    for result in download_results {
                        match result {
                            Err(e) => tracing::error!("Error downloading asset: {:#}", e),
                            Ok(asset_name) => {
                                tracing::info!("Successfully downloaded asset {}", asset_name.display());
                                if let Some(post_download_command) = &mut pieces {
                                    for index in &replace_indices {
                                        post_download_command[*index] = asset_name.display().to_string();
                                    }
            
                                    let mut command = tokio::process::Command::new(post_download_command.get(0).context("empty post-download command")?);
                                    command.args(&post_download_command[1..]);
            
                                    let mut child = command.spawn().context("failed to spawn post-download process")?;
                                    let exit_status = child.wait().await.context("failed to wait on child process")?;
                                    if !exit_status.success() {
                                        return Err(anyhow::anyhow!("post-download command failed to execute successfully: {:?} (exit code: {})", pieces, exit_status));
                                    }
                                }
                            }
                        }
                    }
                },
                _ = shutdown.recv() => break,
            }
        }

        Ok(())
    }
}

#[derive(Debug, serde::Deserialize)]
struct Repo {
    user: String,
    name: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(untagged)]
enum Either<T, U> {
    Left(T),
    Right(U),
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt().with_env_filter(tracing_subscriber::EnvFilter::from_default_env()).init();

    let config_path = std::env::args().nth(1).context("config path not passed as an argument")?;
    let config_contents = std::fs::read_to_string(config_path).context("failed to read config file")?;
    let mut config: Config = toml::from_str(&config_contents).context("failed to deserialize config file contents")?;
    let project_dir = directories::BaseDirs::new().context("failed to get data directory path")?.data_dir().join("gh-release-watcher");

    if !project_dir.exists() {
        std::fs::create_dir(&project_dir).context("failed to create project data directory")?;
    }

    let sled_db = Arc::new(sled::open(project_dir.join("repo-history.db")).context("failed to open or create sled DB")?);
    let mut tasks = Vec::with_capacity(config.repos.len());
    let (tx, _rx) = tokio::sync::broadcast::channel(1);
    let client = reqwest::Client::new();

    for (name, mut repo) in config.repos.drain() {
        repo.interval.get_or_insert(config.options.interval);
        tasks.push(tokio::spawn(repo.watch(name, config.options.timeout, tx.subscribe(), client.clone(), Arc::clone(&sled_db))));
    }

    futures::stream::iter(tasks).for_each_concurrent(None, |result| async move {
        if let Err(e) = result.await.unwrap() {
            tracing::error!("error running watcher: {:#}", e);
        }
    }).await;

    Ok(())
}
