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
    /// Print this device's ID (creating its key on first use) and, once certified, its user.
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
    /// Control API tools.
    Api {
        #[command(subcommand)]
        command: ApiCommand,
    },
    /// Web peer administration.
    Web {
        #[command(subcommand)]
        command: WebCommand,
    },
    /// Run the daemon.
    Run {
        /// Configuration file.
        #[arg(long)]
        config: PathBuf,
    },
}

#[derive(Subcommand)]
enum WebCommand {
    /// Web accounts (stored in `<data_dir>/web/accounts.json`).
    #[command(subcommand)]
    Account(AccountCommand),
}

#[derive(Subcommand)]
enum AccountCommand {
    /// Add an account, or change an existing one. Reads the password from standard input.
    Add {
        /// State directory.
        #[arg(long)]
        data_dir: PathBuf,
        /// Login name.
        #[arg(long)]
        username: String,
        /// The Tidefell user the account acts for. Defaults to this device's user.
        #[arg(long)]
        user: Option<String>,
        /// Allow managing accounts in the web UI.
        #[arg(long)]
        admin: bool,
    },
    /// Remove an account.
    Remove {
        /// State directory.
        #[arg(long)]
        data_dir: PathBuf,
        /// Login name.
        #[arg(long)]
        username: String,
    },
    /// List accounts.
    List {
        /// State directory.
        #[arg(long)]
        data_dir: PathBuf,
    },
}

#[derive(Subcommand)]
enum ApiCommand {
    /// Print the control API's OpenAPI document.
    Schema,
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
    fs::create_dir_all(data_dir).map_err(|e| format!("{}: {e}", data_dir.display()))?;
    let key = DeviceKey::load_or_generate(&device_key_path(data_dir)).map_err(|e| e.to_string())?;
    println!("device: {}", key.id());
    match load_identity(data_dir) {
        Ok((_, cert)) => {
            let body = cert.verify_certificate().map_err(|e| e.to_string())?;
            println!("user:   {}", body.user);
            println!("name:   {}", body.name);
        }
        Err(_) => println!("not certified yet: see `tidefelld init`"),
    }
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

fn web_account(command: AccountCommand) -> Result<(), String> {
    match command {
        AccountCommand::Add {
            data_dir,
            username,
            user,
            admin,
        } => {
            let user = match user {
                Some(user) => user
                    .parse()
                    .map_err(|_| format!("user {user:?} is not 64 lowercase hex digits"))?,
                None => {
                    let (_, cert) = load_identity(&data_dir)?;
                    cert.verify_certificate().map_err(|e| e.to_string())?.user
                }
            };
            eprint!("Password for {username}: ");
            let mut password = String::new();
            std::io::stdin()
                .read_line(&mut password)
                .map_err(|e| e.to_string())?;
            let password = password.trim_end_matches(['\r', '\n']);
            tidefell_web::AccountStore::new(&data_dir)
                .upsert(&username, password, user, admin)
                .map_err(|e| e.to_string())?;
            eprintln!();
            println!("saved account {username} for user {user}");
            Ok(())
        }
        AccountCommand::Remove { data_dir, username } => {
            if tidefell_web::AccountStore::new(&data_dir)
                .remove(&username)
                .map_err(|e| e.to_string())?
            {
                println!("removed account {username}");
                Ok(())
            } else {
                Err(format!("there is no account {username}"))
            }
        }
        AccountCommand::List { data_dir } => {
            for account in tidefell_web::AccountStore::new(&data_dir)
                .list()
                .map_err(|e| e.to_string())?
            {
                let role = if account.admin { "admin" } else { "member" };
                println!("{}  {}  {role}", account.username, account.user);
            }
            Ok(())
        }
    }
}

fn read_config(path: &Path) -> Result<config::Config, String> {
    let text = fs::read_to_string(path).map_err(|e| format!("{}: {e}", path.display()))?;
    config::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
}

async fn run(path: &Path) -> Result<(), String> {
    let config = read_config(path)?;
    let engine_config = config.to_engine()?;
    let (key, cert) = load_identity(&config.data_dir)?;
    let engine = std::sync::Arc::new(
        Engine::start(engine_config, key, cert)
            .await
            .map_err(|e| e.to_string())?,
    );
    tracing::info!(device = %engine.id(), "tidefelld running");

    let socket = tidefell_sdk::Client::socket_path(&config.data_dir);
    let api = tokio::spawn({
        let (engine, socket) = (engine.clone(), socket.clone());
        async move { tidefell_api::serve_unix(engine, &socket).await }
    });
    let web = match &config.web {
        Some(web) => {
            let listener = tokio::net::TcpListener::bind(web.bind)
                .await
                .map_err(|e| format!("web peer cannot bind {}: {e}", web.bind))?;
            let settings = tidefell_web::WebConfig {
                secure_cookies: web.secure_cookies,
            };
            let (engine, data_dir) = (engine.clone(), config.data_dir.clone());
            tokio::spawn(
                async move { tidefell_web::serve(listener, engine, &data_dir, settings).await },
            )
        }
        None => tokio::spawn(std::future::pending()),
    };
    let describe =
        |what: &str, result: Result<std::io::Result<()>, tokio::task::JoinError>| match result {
            Ok(Ok(())) => format!("{what} stopped"),
            Ok(Err(e)) => format!("{what} failed: {e}"),
            Err(e) => format!("{what} task failed: {e}"),
        };
    tokio::select! {
        _ = tokio::signal::ctrl_c() => tracing::info!("shutting down"),
        result = api => return Err(describe("control API", result)),
        result = web => return Err(describe("web peer", result)),
    }
    let _ = std::fs::remove_file(&socket);

    // In-flight API requests hold the engine briefly; wait for them before a clean shutdown.
    let mut engine = engine;
    for _ in 0..50 {
        match std::sync::Arc::try_unwrap(engine) {
            Ok(engine) => {
                engine.shutdown().await;
                return Ok(());
            }
            Err(shared) => {
                engine = shared;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
        }
    }
    tracing::warn!("API requests still running; exiting without a clean engine shutdown");
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
        Command::Web { command } => match command {
            WebCommand::Account(command) => web_account(command),
        },
        Command::Api { command } => match command {
            ApiCommand::Schema => tidefell_api::openapi()
                .to_pretty_json()
                .map(|json| println!("{json}"))
                .map_err(|e| e.to_string()),
        },
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
