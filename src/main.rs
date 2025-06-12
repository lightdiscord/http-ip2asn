#![feature(btree_cursors)]

use axum::Router;
use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::routing::get;
use clap::Parser;
use flate2::read::GzDecoder;
use std::collections::{BTreeMap, Bound};
use std::io::Read;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::str::FromStr;
use std::sync::Arc;
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use log::info;
use reqwest::Url;

#[derive(Parser)]
#[command(version, about, long_about = None)]
struct Cli {
    /// Local database file path to use instead of the URL.
    #[arg(long, env = "IPTOASN_DATABASE_FILE")]
    database_file: Option<PathBuf>,

    /// Download URL for the database file to be fetched regularly.
    #[arg(long, env = "IPTOASN_DATABASE_URL", default_value = "https://iptoasn.com/data/ip2asn-combined.tsv.gz")]
    database_url: Url,

    /// Database retrieval frequency in seconds
    #[arg(long, env = "IPTOASN_DATABASE_FREQUENCY", default_value = "3600")]
    database_frequency: usize,

    /// Host used for the web server
    #[arg(long, env = "HOST", default_value = "0.0.0.0")]
    host: IpAddr,

    /// Port used for the web server
    #[arg(long, env = "PORT", default_value = "80")]
    port: u16,
}

fn gunzip(bytes: Vec<u8>) -> std::io::Result<String> {
    let mut gz = GzDecoder::new(&bytes[..]);
    let mut s = String::new();
    gz.read_to_string(&mut s)?;
    Ok(s)
}

#[derive(Debug)]
struct ASN {
    range_start: IpAddr,
    range_end: IpAddr,
    as_number: u32,
    country_code: String,
    description: String,
}

struct Database {
    inner: BTreeMap<IpAddr, ASN>,
}

impl Database {
    fn new(inner: BTreeMap<IpAddr, ASN>) -> Self {
        Self { inner }
    }

    pub fn get(&self, address: IpAddr) -> Option<&ASN> {
        self.inner.upper_bound(Bound::Included(&address))
            .peek_prev()
            .map(|(_index, asn)| asn)
            .filter(|asn| asn.range_start <= address && address <= asn.range_end)
    }
}

struct AppState {
    database: Option<Database>,
}

// TODO: Change error
fn load_asns(contents: String) -> Result<Database, Box<dyn std::error::Error>> {
    let mut map = BTreeMap::new();

    for line in contents.lines() {
        let mut parts = line.splitn(5, '\t');

        let range_start: IpAddr = parts.next().ok_or("Missing range start")?.parse()?;
        let range_end: IpAddr = parts.next().ok_or("Missing range end")?.parse()?;
        let as_number: u32 = parts.next().ok_or("Missing AS number")?.parse()?;
        let country_code = parts.next().ok_or("Missing country code")?;
        let description = parts.next().ok_or("Missing AS description")?;

        let asn = ASN {
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

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();

    let cli = Cli::parse();

    let mut file = File::open(cli.database_file.unwrap()).await?;
    let mut contents = vec![];
    file.read_to_end(&mut contents).await?;

    info!("Parsing database file");

    let contents = gunzip(contents)?;

    let state = AppState {
        database: Some(load_asns(contents)?),
    };

    let state = Arc::new(state);

    let app = Router::new()
        .route("/", get(root))
        .with_state(state)
        .into_make_service_with_connect_info::<SocketAddr>();

    let listen_address = SocketAddr::new(cli.host, cli.port);
    let listener = tokio::net::TcpListener::bind(listen_address).await.unwrap();
    axum::serve(listener, app).await.unwrap();

    Ok(())
}

async fn root(
    ConnectInfo(address): ConnectInfo<SocketAddr>,
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
) -> (StatusCode, HeaderMap) {
    if let Some(database) = state.database.as_ref() {
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
