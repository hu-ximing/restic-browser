use std::{fs::OpenOptions, path::PathBuf, sync::Arc};

use clap::{Parser, ValueEnum};
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use restic_browser::{
    AppError, Result, app::App, cache::SessionCache, error::redact, language::Language,
    preview::PreviewService, repository::RepositoryHandle, restic::ResticCliClient,
    rustic::RusticClient, terminal,
};

#[derive(Debug, Clone, Copy, Default, ValueEnum)]
enum Backend {
    #[default]
    Rustic,
    ResticCli,
}

#[derive(Debug, Parser)]
#[command(
    name = "restic-browser",
    version,
    about = "Browse restic snapshots, preview and export files, or restore directories (read-only)"
)]
struct Cli {
    #[arg(short = 'r', long, env = "RESTIC_REPOSITORY")]
    repository: Option<PathBuf>,
    #[arg(long, value_enum, default_value_t)]
    backend: Backend,
    #[arg(long, default_value = "restic")]
    restic: PathBuf,
    #[arg(long, default_value = "ffmpeg")]
    ffmpeg: PathBuf,
    #[arg(long, default_value = "ffprobe")]
    ffprobe: PathBuf,
    #[arg(long)]
    log_file: Option<PathBuf>,
    /// Display the interface in Chinese.
    #[arg(long)]
    cn: bool,
}

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("restic-browser: {error}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let cli = Cli::parse();
    let language = Language::from_chinese_flag(cli.cn);
    init_logging(cli.log_file.as_ref(), language)?;
    let repository = cli.repository.ok_or_else(|| {
        AppError::Other(
            language
                .text(
                    "specify a local repository with --repository",
                    "请使用 --repository 指定本地仓库",
                )
                .to_owned(),
        )
    })?;

    if matches!(cli.backend, Backend::ResticCli) {
        ResticCliClient::check_version(&cli.restic).await?;
    }
    let cache = SessionCache::new()?;
    let preview_service = PreviewService::new(cli.ffmpeg, cli.ffprobe, cache);

    rtoolbox::print_tty::print_tty(language.text("Repository password: ", "仓库密码: "))
        .map_err(AppError::Io)?;
    let password = rpassword::read_password().map_err(AppError::Io)?;
    let (client, restore_client): (RepositoryHandle, Arc<ResticCliClient>) = match cli.backend {
        Backend::Rustic => {
            let restore_client = Arc::new(ResticCliClient::new(
                cli.restic,
                repository.clone(),
                SecretString::from(password.clone()),
            )?);
            (
                Arc::new(RusticClient::open(repository, password)?),
                restore_client,
            )
        }
        Backend::ResticCli => {
            let restic_client = Arc::new(ResticCliClient::new(
                cli.restic,
                repository,
                SecretString::from(password),
            )?);
            (restic_client.clone(), restic_client)
        }
    };
    let snapshots = client
        .list_snapshots(CancellationToken::new())
        .await
        .inspect_err(|error| {
            tracing::error!("{}", redact(&error.to_string()));
        })?;

    let app = App::new(
        client,
        restore_client,
        Arc::new(preview_service),
        snapshots,
        language,
    );
    terminal::run(app).await
}

fn init_logging(path: Option<&PathBuf>, language: Language) -> Result<()> {
    let Some(path) = path else {
        return Ok(());
    };
    let file = OpenOptions::new().create(true).append(true).open(path)?;
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .with_ansi(false)
        .with_writer(move || {
            file.try_clone()
                .expect("diagnostic log file should remain available")
        })
        .try_init()
        .map_err(|error| {
            AppError::Other(format!(
                "{}: {error}",
                language.text("failed to enable logging", "无法启用日志")
            ))
        })?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interface_language_defaults_to_english() {
        let cli = Cli::try_parse_from(["restic-browser"]).unwrap();

        assert!(!cli.cn);
    }

    #[test]
    fn cn_flag_enables_chinese() {
        let cli = Cli::try_parse_from(["restic-browser", "--cn"]).unwrap();

        assert!(cli.cn);
    }
}
