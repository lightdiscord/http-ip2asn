#![feature(btree_cursors)]

use crate::DatabaseError::{
    MissingASDescription, MissingASNumber, MissingCountryCode, MissingRangeEnd, MissingRangeStart,
};
use arc_swap::ArcSwapOption;
use axum::Router;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use clap::Parser;
use flate2::read::GzDecoder;
use log::{debug, error, info};
use reqwest::Url;
use std::collections::{BTreeMap, Bound};
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use thiserror::Error;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::time::sleep;
use tokio::{task, try_join};

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Local database file path to use instead of the URL.
    #[arg(long, env = "IPTOASN_DATABASE_FILE")]
    database_file: Option<PathBuf>,

    /// Download URL for the database file to be fetched regularly.
    #[arg(
        long,
        env = "IPTOASN_DATABASE_URL",
        default_value = "https://iptoasn.com/data/ip2asn-combined.tsv.gz"
    )]
    database_url: Url,

    /// Database retrieval frequency in seconds (0 to disable)
    #[arg(long, env = "IPTOASN_DATABASE_FREQUENCY", default_value = "0")]
    database_frequency: u64,

    /// Host used for the web server
    #[arg(long, env = "HOST", default_value = "0.0.0.0")]
    host: IpAddr,

    /// Port used for the web server
    #[arg(long, env = "PORT", default_value = "80")]
    port: u16,
}

#[derive(Debug)]
struct Asn {
    range_start: IpAddr,
    range_end: IpAddr,
    as_number: u32,
    country_code: String,
    description: String,
}

struct Database {
    inner: BTreeMap<IpAddr, Asn>,
}

impl Database {
    fn new(inner: BTreeMap<IpAddr, Asn>) -> Self {
        Self { inner }
    }

    pub fn get(&self, address: IpAddr) -> Option<&Asn> {
        self.inner
            .upper_bound(Bound::Included(&address))
            .peek_prev()
            .map(|(_index, asn)| asn)
            .filter(|asn| asn.range_start <= address && address <= asn.range_end)
    }
}

struct AppState {
    options: Cli,
    database: ArcSwapOption<Database>,
}

#[derive(Error, Debug)]
pub enum DatabaseError {
    #[error("IO error")]
    IoError(#[from] std::io::Error),

    #[error("Invalid IP address")]
    AddrParseError(#[from] std::net::AddrParseError),

    #[error("Parse integer error")]
    ParseIntError(#[from] std::num::ParseIntError),

    #[error("Missing range start")]
    MissingRangeStart,

    #[error("Missing range end")]
    MissingRangeEnd,

    #[error("Missing AS number")]
    MissingASNumber,

    #[error("Missing country code")]
    MissingCountryCode,

    #[error("Missing AS description")]
    MissingASDescription,
}

fn gunzip(bytes: Vec<u8>) -> Result<String, DatabaseError> {
    let mut gz = GzDecoder::new(&bytes[..]);
    let mut s = String::new();
    gz.read_to_string(&mut s)?;
    Ok(s)
}

fn load_asns(contents: String) -> Result<Database, DatabaseError> {
    let mut map = BTreeMap::new();

    for line in contents.lines() {
        let mut parts = line.splitn(5, '\t');

        let range_start: IpAddr = parts.next().ok_or(MissingRangeStart)?.parse()?;
        let range_end: IpAddr = parts.next().ok_or(MissingRangeEnd)?.parse()?;
        let as_number: u32 = parts.next().ok_or(MissingASNumber)?.parse()?;
        let country_code = parts.next().ok_or(MissingCountryCode)?;
        let description = parts.next().ok_or(MissingASDescription)?;

        let asn = Asn {
            range_start,
            range_end,
            as_number,
            country_code: country_code.to_string(),
            description: description.to_string(),
        };

        map.insert(range_start, asn);
    }

    Ok(Database::new(map))
}

async fn database_synchronization_once(
    options: &Cli,
) -> Result<Database, Box<dyn std::error::Error>> {
    let database = if let Some(path) = &options.database_file {
        let mut file = File::open(path).await?;
        let mut contents = vec![];
        file.read_to_end(&mut contents).await?;

        contents
    } else {
        reqwest::get(options.database_url.clone())
            .await?
            .error_for_status()?
            .bytes()
            .await?
            .to_vec()
    };

    // The database can be huge which can blocks requests on the web server.
    let database = task::spawn_blocking(move || {
        let database = gunzip(database);

        match database {
            Ok(database) => load_asns(database),
            Err(error) => Err(error),
        }
    })
    .await?;

    Ok(database?)
}

async fn database_synchronization(state: Arc<AppState>) -> Result<(), Box<dyn std::error::Error>> {
    loop {
        debug!("Starting database synchronization");

        match database_synchronization_once(&state.options).await {
            Ok(database) => {
                state.database.store(Some(Arc::new(database)));

                info!("Database has been synchronized");

                // If the frequency is disabled, stops after the first success.
                if state.options.database_frequency == 0 {
                    return Ok(());
                }

                sleep(Duration::from_secs(state.options.database_frequency)).await;
            }
            Err(e) => {
                error!("error while synchronizing database: {}", e);

                // TODO: Exponential back-off.

                sleep(Duration::from_secs(15)).await;
            }
        }
    }
}

async fn webserver(state: Arc<AppState>) -> Result<(), Box<dyn std::error::Error>> {
    let listen_address = SocketAddr::new(state.options.host, state.options.port);
    let app = Router::new()
        .route("/", get(root))
        .with_state(state.clone())
        .into_make_service_with_connect_info::<SocketAddr>();

    let listener = tokio::net::TcpListener::bind(listen_address).await?;
    info!("Server listening {}", listen_address);
    axum::serve(listener, app).await?;

    Ok(())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();

    let cli = Cli::parse();

    let state = Arc::new(AppState {
        options: cli,
        database: ArcSwapOption::from(None),
    });

    try_join!(webserver(state.clone()), database_synchronization(state))?;

    Ok(())
}

async fn root(
    ConnectInfo(address): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> (StatusCode, HeaderMap) {
    let database = state.database.load();

    if let Some(database) = &*database {
        let address = headers
            .get("X-Forwarded-For")
            .map(|addr| IpAddr::from_str(addr.to_str().unwrap()))
            .unwrap_or(Ok(address.ip()))
            .unwrap();

        if let Some(asn) = database.get(address) {
            let mut headers = HeaderMap::new();

            headers.insert("x-asn-number", asn.as_number.to_string().parse().unwrap());
            headers.insert("x-asn-country", asn.country_code.parse().unwrap());
            headers.insert("x-asn-description", asn.description.parse().unwrap());

            (StatusCode::OK, headers)
        } else {
            (StatusCode::NOT_FOUND, HeaderMap::new())
        }
    } else {
        (StatusCode::SERVICE_UNAVAILABLE, HeaderMap::new())
    }
}
