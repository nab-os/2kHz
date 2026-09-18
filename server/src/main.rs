//! `two-khz-server`: the half that owns the credentials and the machine.
//!
//! Holds the Qobuz token, the shared rate limit, the database, the CLAP text
//! tower and the pipeline stages. Clients get a slim catalogue and the vectors.
//!
//! ```sh
//! two-khz-server pair --name desktop --scope pipeline   # first device
//! two-khz-server serve                                  # 127.0.0.1:7700
//! ```
//!
//! Plain HTTP, loopback by default. The token and the signed stream URLs are
//! credentials in flight, so anything further belongs behind a VPN or TLS.

mod auth;
mod catalog;
mod routes;

use anyhow::{Context, Result};
use auth::AuthStore;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use two_khz::api::Scope;
use two_khz::backend::Local;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

const DEFAULT_BIND: &str = "127.0.0.1:7700";

#[derive(Clone)]
pub struct AppState {
    /// The same type the desktop app uses locally. There is deliberately no
    /// second implementation of the crawl loop or the stage runner here.
    pub local: Arc<Local>,
    pub auth: Arc<AuthStore>,
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
}

// ------------------------------------------------------------------ errors

/// A failed request, as the client can turn back into an `anyhow::Error`.
pub struct Failure {
    status: StatusCode,
    message: String,
}

impl Failure {
    pub fn unauthorised(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            message: message.into(),
        }
    }

    pub fn forbidden(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::FORBIDDEN,
            message: message.into(),
        }
    }

    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: message.into(),
        }
    }
}

/// Report what actually went wrong. Deliberately not sanitised: single-user
/// system behind a VPN, and a real message beats sending someone to the logs
/// on another machine.
impl From<anyhow::Error> for Failure {
    fn from(err: anyhow::Error) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: format!("{err:#}"),
        }
    }
}

impl IntoResponse for Failure {
    fn into_response(self) -> Response {
        (
            self.status,
            axum::Json(two_khz::api::ApiError {
                message: self.message,
            }),
        )
            .into_response()
    }
}

// --------------------------------------------------------------------- cli

fn usage() -> ! {
    eprintln!(
        "\
two-khz-server: the server half of 2kHz

  serve [--bind ADDR]            run the API (default {DEFAULT_BIND})
  pair --name NAME [--scope S]   mint a device token; S is play|pipeline
  devices                        list paired devices
  revoke ID                      revoke one device
  build-catalog                  rebuild the slim catalogue clients sync

Devices are stored in the same database as the catalogue. A token is shown
once, at pairing, and only its hash is kept."
    );
    std::process::exit(2);
}

fn flag(args: &[String], name: &str) -> Option<String> {
    let index = args.iter().position(|a| a == name)?;
    args.get(index + 1).cloned()
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(command) = args.first().map(|s| s.as_str()) else {
        usage()
    };

    let data_dir = two_khz::default_data_dir();
    let db_path = two_khz::default_db_path();
    let store = AuthStore::new(&db_path)?;

    match command {
        "serve" => {
            let bind = flag(&args, "--bind").unwrap_or_else(|| DEFAULT_BIND.to_string());
            serve(bind, data_dir, db_path, store)
        }
        "pair" => {
            let Some(name) = flag(&args, "--name") else {
                eprintln!("pair needs --name");
                std::process::exit(2);
            };
            let scope = flag(&args, "--scope")
                .map(|s| {
                    Scope::parse(&s).unwrap_or_else(|| {
                        eprintln!("unknown scope “{s}”; use play or pipeline");
                        std::process::exit(2);
                    })
                })
                .unwrap_or(Scope::Play);

            let grant = store.issue(&name, scope)?;
            println!(
                "Paired “{}” with scope {}.\n\nSet this on the device, it is not shown again:\n\n  \
                 export TWO_KHZ_SERVER=http://<this-host>:7700\n  \
                 export TWO_KHZ_TOKEN={}\n",
                grant.device.name,
                scope.as_str(),
                grant.token
            );
            Ok(())
        }
        "devices" => {
            let devices = store.list()?;
            if devices.is_empty() {
                println!("No devices paired. Start with:\n  two-khz-server pair --name desktop --scope pipeline");
            }
            for device in devices {
                println!(
                    "{:>4}  {:<24} {:<9} last seen {}",
                    device.id,
                    device.name,
                    device.scope.as_str(),
                    device.last_seen.as_deref().unwrap_or("never")
                );
            }
            Ok(())
        }
        "revoke" => {
            let Some(id) = args.get(1).and_then(|v| v.parse::<i64>().ok()) else {
                eprintln!("revoke needs a device id; see `two-khz-server devices`");
                std::process::exit(2);
            };
            store.revoke(id)?;
            println!("Revoked device {id}.");
            Ok(())
        }
        "build-catalog" => {
            let target = data_dir.join("catalog.db");
            let bytes = catalog::build(&db_path, &target)?;
            println!(
                "Wrote {} ({:.1} MB) from {}.",
                target.display(),
                bytes as f64 / 1_048_576.0,
                db_path.display()
            );
            Ok(())
        }
        _ => usage(),
    }
}

fn serve(bind: String, data_dir: PathBuf, db_path: PathBuf, store: AuthStore) -> Result<()> {
    let address: SocketAddr = bind
        .parse()
        .with_context(|| format!("“{bind}” is not an address:port"))?;

    if !address.ip().is_loopback() {
        eprintln!(
            "WARNING: binding to {address}, which is not loopback.\n\
             This speaks plain HTTP. The device tokens and the signed stream URLs\n\
             it hands out are both credentials, so put it behind WireGuard/Tailscale\n\
             or a TLS proxy, do not expose it directly.\n"
        );
    }

    if store.count()? == 0 {
        eprintln!(
            "No devices are paired, so every request will be refused. Mint one with:\n  \
             two-khz-server pair --name desktop --scope pipeline\n"
        );
    }

    // A stage must not outlive the server.
    two_khz::stages::install_exit_guard();

    let local = Arc::new(Local::new(
        two_khz::qobuz::repo_root(),
        db_path.clone(),
        two_khz::default_model_dir(),
    ));

    let state = AppState {
        local: local.clone(),
        auth: Arc::new(store),
        data_dir: data_dir.clone(),
        db_path: db_path.clone(),
    };

    let runtime = tokio::runtime::Runtime::new()?;
    runtime.block_on(async move {
        // The slim catalogue has to follow the space: a client syncing new
        // vectors against an old catalogue draws the right points with the
        // wrong labels.
        tokio::spawn(watch_generation(local, db_path, data_dir));

        let listener = tokio::net::TcpListener::bind(address).await?;
        println!("two-khz-server listening on http://{address}");

        axum::serve(listener, routes::router(state))
            .with_graceful_shutdown(shutdown())
            .await?;
        Ok::<(), anyhow::Error>(())
    })?;

    Ok(())
}

/// Rebuild `catalog.db` whenever a stage has rewritten the space.
async fn watch_generation(local: Arc<Local>, db_path: PathBuf, data_dir: PathBuf) {
    let mut seen = u64::MAX;

    loop {
        if let Ok(status) = local.pipeline_status().await {
            if status.generation != seen {
                // Skip the rebuild on the first pass if one already exists;
                // `build-catalog` covers the cold start.
                if seen != u64::MAX || !data_dir.join("catalog.db").exists() {
                    let target = data_dir.join("catalog.db");
                    match catalog::build(&db_path, &target) {
                        Ok(bytes) => println!(
                            "rebuilt {} ({:.1} MB)",
                            target.display(),
                            bytes as f64 / 1_048_576.0
                        ),
                        Err(err) => eprintln!("could not rebuild the slim catalogue: {err:#}"),
                    }
                }
                seen = status.generation;
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
    }
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    println!("\nshutting down; any running stage is being signalled");
}
