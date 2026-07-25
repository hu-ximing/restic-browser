use std::{fs::OpenOptions, path::PathBuf, sync::Arc};

use clap::{Parser, ValueEnum};
use secrecy::SecretString;
use tokio_util::sync::CancellationToken;
use tracing_subscriber::EnvFilter;

use restic_browser::{
    AppError, Result, app::App, cache::SessionCache, error::redact, preview::PreviewService,
    repository::RepositoryHandle, restic::ResticCliClient, rustic::RusticClient, terminal,
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
    about = "只读浏览 restic 快照、预览并导出单个文件"
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
    init_logging(cli.log_file.as_ref())?;
    let repository = cli
        .repository
        .ok_or_else(|| AppError::Other("请使用 --repository 指定本地仓库".to_owned()))?;

    if matches!(cli.backend, Backend::ResticCli) {
        ResticCliClient::check_version(&cli.restic).await?;
    }
    let cache = SessionCache::new()?;
    let preview_service = PreviewService::new(cli.ffmpeg, cli.ffprobe, cache);
    preview_service.check_dependencies().await?;

    rtoolbox::print_tty::print_tty("仓库密码: ").map_err(AppError::Io)?;
    let password = rpassword::read_password().map_err(AppError::Io)?;
    let client: RepositoryHandle = match cli.backend {
        Backend::Rustic => Arc::new(RusticClient::open(repository, password)?),
        Backend::ResticCli => Arc::new(ResticCliClient::new(
            cli.restic,
            repository,
            SecretString::from(password),
        )?),
    };
    let snapshots = client
        .list_snapshots(CancellationToken::new())
        .await
        .inspect_err(|error| {
            tracing::error!("{}", redact(&error.to_string()));
        })?;

    let app = App::new(client, Arc::new(preview_service), snapshots);
    terminal::run(app).await
}

fn init_logging(path: Option<&PathBuf>) -> Result<()> {
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
        .map_err(|error| AppError::Other(format!("无法启用日志：{error}")))?;
    Ok(())
}
