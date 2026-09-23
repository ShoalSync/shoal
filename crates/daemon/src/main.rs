//! `tidefelld`: the headless Tidefell peer for Linux servers and NAS devices.

mod config;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use clap::{Parser, Subcommand};
use tidefell_engine::Engine;
use tidefell_identity::{load_certificate, save_certificate, DeviceKey, UserKey};
use tidefell_proto::{DeviceId, FolderId};

#[derive(Parser)]
#[command(name = "tidefelld", version, about = "Tidefell peer daemon")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Create this device's key and certificate in a data directory.
    Init {
        /// State directory (the `data_dir` of the configuration).
        #[arg(long)]
        data_dir: PathBuf,
        /// Human-readable device name.
        #[arg(long)]
        name: String,
        /// Certify with this user key, creating it if missing. Defaults to
        /// `<data_dir>/user.key`. Back this file up: it proves which devices are yours.
        #[arg(long, conflicts_with = "certificate")]
        user_key: Option<PathBuf>,
        /// Use a certificate issued elsewhere with `tidefelld certify` instead.
        #[arg(long)]
        certificate: Option<PathBuf>,
    },
    /// Issue a certificate for another device, to copy to it and pass to `init --certificate`.
    Certify {
        /// The user key file.
        #[arg(long)]
        user_key: PathBuf,
        /// The other device's ID, as printed by `tidefelld id`.
        #[arg(long)]
        device: String,
        /// Human-readable device name.
        #[arg(long)]
        name: String,
        /// Where to write the certificate.
        #[arg(long)]
        out: PathBuf,
    },
    /// Print this device's ID and its user's ID.
    Id {
        /// State directory.
        #[arg(long)]
        data_dir: PathBuf,
    },
    /// Print a new random folder ID.
    NewFolderId,
    /// Configuration file tools.
    Config {
        #[command(subcommand)]
        command: ConfigCommand,
    },
    /// Run the daemon.
    Run {
        /// Configuration file.
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum ConfigCommand {
    /// Print the configuration file's JSON Schema.
    Schema,
    /// Validate a configuration file.
    Check {
        /// Configuration file.
        file: PathBuf,
    },
}

fn device_key_path(data_dir: &Path) -> PathBuf {
    data_dir.join("device.key")
}

fn certificate_path(data_dir: &Path) -> PathBuf {
    data_dir.join("device.cert")
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn init(
    data_dir: &Path,
    name: &str,
    user_key: Option<PathBuf>,
    certificate: Option<PathBuf>,
) -> Result<(), String> {
    fs::create_dir_all(data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
    let device =
        DeviceKey::load_or_generate(&device_key_path(data_dir)).map_err(|e| e.to_string())?;
    let cert = match certificate {
        Some(path) => {
            let cert = load_certificate(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            let body = cert.verify_certificate().map_err(|e| e.to_string())?;
            if body.device != device.id() {
                return Err(format!(
                    "certificate is for device {}, but this device is {}",
                    body.device,
                    device.id()
                ));
            }
            cert
        }
        None => {
            let path = user_key.unwrap_or_else(|| data_dir.join("user.key"));
            let user =
                UserKey::load_or_generate(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            user.certify(device.id(), name, now_ms())
        }
    };
    save_certificate(&certificate_path(data_dir), &cert).map_err(|e| e.to_string())?;
    print_id(data_dir)
}

fn load_identity(
    data_dir: &Path,
) -> Result<
    (
        DeviceKey,
        tidefell_proto::Signed<tidefell_proto::identity::DeviceCertificate>,
    ),
    String,
> {
    let key = DeviceKey::load(&device_key_path(data_dir)).map_err(|e| {
        format!(
            "no device key in {} ({e}); run `tidefelld init`",
            data_dir.display()
        )
    })?;
    let cert = load_certificate(&certificate_path(data_dir)).map_err(|e| {
        format!(
            "no certificate in {} ({e}); run `tidefelld init`",
            data_dir.display()
        )
    })?;
    let body = cert.verify_certificate().map_err(|e| e.to_string())?;
    if body.device != key.id() {
        return Err("certificate does not match the device key".into());
    }
    Ok((key, cert))
}

fn print_id(data_dir: &Path) -> Result<(), String> {
    let (key, cert) = load_identity(data_dir)?;
    let body = cert.verify_certificate().map_err(|e| e.to_string())?;
    println!("device: {}", key.id());
    println!("user:   {}", body.user);
    println!("name:   {}", body.name);
    Ok(())
}

fn certify(user_key: &Path, device: &str, name: &str, out: &Path) -> Result<(), String> {
    let user = UserKey::load(user_key).map_err(|e| format!("{}: {e}", user_key.display()))?;
    let device: DeviceId = device
        .parse()
        .map_err(|_| format!("device {device:?} is not 64 lowercase hex digits"))?;
    save_certificate(out, &user.certify(device, name, now_ms())).map_err(|e| e.to_string())?;
    println!("wrote {}", out.display());
    Ok(())
}

fn read_config(path: &Path) -> Result<config::Config, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    config::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

async fn run(path: &Path) -> Result<(), String> {
    let config = read_config(path)?;
    let engine_config = config.to_engine()?;
    let (key, cert) = load_identity(&config.data_dir)?;
    let engine = Engine::start(engine_config, key, cert)
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!(device = %engine.id(), "tidefelld running");
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("shutting down");
    engine.shutdown().await;
    Ok(())
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let result = match cli.command {
        Command::Init {
            data_dir,
            name,
            user_key,
            certificate,
        } => init(&data_dir, &name, user_key, certificate),
        Command::Certify {
            user_key,
            device,
            name,
            out,
        } => certify(&user_key, &device, &name, &out),
        Command::Id { data_dir } => print_id(&data_dir),
        Command::NewFolderId => {
            let mut bytes = [0u8; 32];
            getrandom::fill(&mut bytes)
                .map(|()| println!("{}", FolderId(bytes)))
                .map_err(|e| e.to_string())
        }
        Command::Config { command } => match command {
            ConfigCommand::Schema => {
                println!("{}", config::schema());
                Ok(())
            }
            ConfigCommand::Check { file } => read_config(&file)
                .and_then(|c| c.to_engine().map(|_| ()))
                .map(|()| println!("{}: ok", file.display())),
        },
        Command::Run { config } => {
            tracing_subscriber::fmt()
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| "info".into()),
                )
                .init();
            tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| e.to_string())
                .and_then(|rt| rt.block_on(run(&config)))
        }
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("tidefelld: {e}");
            ExitCode::FAILURE
        }
    }
}
