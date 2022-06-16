#[macro_use]
extern crate log;

use actix_web::{get, http::header::ContentType, web, App, HttpResponse, HttpServer};
use anyhow::anyhow;
use clap::Parser;
use hex::ToHex;
use secp256k1_zkp::{rand, KeyPair, Secp256k1, SecretKey};
use serde::{Deserialize, Serialize};
use serde_json::{self, json};
use sled::IVec;
use std::{
    collections::HashMap,
    fs::{self, File},
    io::Read,
    str::FromStr,
};
use time::{format_description::well_known::Rfc3339, Duration, OffsetDateTime};

use sybils::{
    oracle::{
        oracle_scheduler,
        pricefeeds::{Bitstamp, GateIo, Kraken, PriceFeed},
        DbValue, Oracle,
    },
    AssetPair, AssetPairInfo, OracleConfig,
};

const PAGE_SIZE: u32 = 100;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
enum SortOrder {
    Insertion,
    ReverseInsertion,
}

#[derive(Debug, Deserialize)]
#[serde(default, rename_all = "camelCase")]
struct Filters {
    sort_by: SortOrder,
    page: u32,
    asset_pair: AssetPair,
}

impl Default for Filters {
    fn default() -> Self {
        Filters {
            sort_by: SortOrder::ReverseInsertion,
            page: 0,
            asset_pair: AssetPair::BTCUSD,
        }
    }
}

#[derive(Serialize)]
struct ApiOracleEvent {
    asset_pair: AssetPair,
    announcement: String,
    attestation: Option<String>,
    maturation: String,
    outcome: Option<u64>,
}

fn make_api_response<T: Serialize>(result: Option<T>, error: Option<String>) -> HttpResponse {
    HttpResponse::Ok().content_type(ContentType::json()).body(
        json!({
            "result": result,
            "error": error,
        })
        .to_string(),
    )
}

fn parse_database_entry(
    asset_pair: AssetPair,
    (maturation, event): (IVec, IVec),
) -> ApiOracleEvent {
    let maturation = String::from_utf8_lossy(&maturation).to_string();
    let event: DbValue = serde_json::from_str(&String::from_utf8_lossy(&event)).unwrap();
    ApiOracleEvent {
        asset_pair,
        announcement: event.1.encode_hex::<String>(),
        attestation: event.2.map(|att| att.encode_hex::<String>()),
        maturation,
        outcome: event.3,
    }
}

fn execute_announcements(
    oracles: &HashMap<AssetPair, Oracle>,
    filters: &Filters,
) -> anyhow::Result<HttpResponse> {
    let oracle = match oracles.get(&filters.asset_pair) {
        None => {
            return Ok(make_api_response::<String>(
                None,
                Some(format!("asset pair {} not recorded", filters.asset_pair)),
            ))
        }
        Some(val) => val,
    };

    if oracle.event_database.is_empty() {
        info!("no oracle events found");
        return Ok(make_api_response(Some(Vec::<ApiOracleEvent>::new()), None));
    }

    let start = filters.page * PAGE_SIZE;

    match filters.sort_by {
        SortOrder::Insertion => loop {
            let init_key = oracle.event_database.first()?.unwrap().0;
            let start_key = OffsetDateTime::parse(&String::from_utf8_lossy(&init_key), &Rfc3339)
                .unwrap()
                + Duration::days(start.into());
            let end_key = start_key + Duration::days(PAGE_SIZE.into());
            let start_key = start_key.format(&Rfc3339).unwrap().into_bytes();
            let end_key = end_key.format(&Rfc3339).unwrap().into_bytes();
            if init_key == oracle.event_database.first()?.unwrap().0 {
                // don't know if range can change while iterating due to another thread modifying
                info!(
                    "retrieving oracle events from {} to {}",
                    String::from_utf8_lossy(&start_key),
                    String::from_utf8_lossy(&end_key),
                );
                return Ok(make_api_response(
                    Some(
                        oracle
                            .event_database
                            .range(start_key..end_key)
                            .map(|result| parse_database_entry(filters.asset_pair, result.unwrap()))
                            .collect::<Vec<_>>(),
                    ),
                    None,
                ));
            }
        },
        SortOrder::ReverseInsertion => loop {
            let init_key = oracle.event_database.last()?.unwrap().0;
            let end_key = OffsetDateTime::parse(&String::from_utf8_lossy(&init_key), &Rfc3339)
                .unwrap()
                - Duration::days(start.into());
            let start_key = end_key - Duration::days(PAGE_SIZE.into());
            let start_key = start_key.format(&Rfc3339).unwrap().into_bytes();
            let end_key = end_key.format(&Rfc3339).unwrap().into_bytes();
            if init_key == oracle.event_database.last()?.unwrap().0 {
                // don't know if range can change while iterating due to another thread modifying
                info!(
                    "retrieving oracle events from {} to {}",
                    String::from_utf8_lossy(&start_key),
                    String::from_utf8_lossy(&end_key),
                );
                return Ok(make_api_response(
                    Some(
                        oracle
                            .event_database
                            .range(start_key..end_key)
                            .map(|result| parse_database_entry(filters.asset_pair, result.unwrap()))
                            .collect::<Vec<_>>(),
                    ),
                    None,
                ));
            }
        },
    }
}

#[get("/announcements")]
async fn announcements(
    oracles: web::Data<HashMap<AssetPair, Oracle>>,
    filters: web::Query<Filters>,
) -> HttpResponse {
    info!("GET /announcements: {:#?}", filters);
    match execute_announcements(&oracles, &filters) {
        Ok(val) => val,
        Err(err) => make_api_response::<String>(None, Some(err.to_string())),
    }
}

fn execute_announcement(
    oracles: &HashMap<AssetPair, Oracle>,
    filters: &Filters,
    rfc3339_time: &str,
) -> anyhow::Result<HttpResponse> {
    let _ = OffsetDateTime::parse(rfc3339_time, &Rfc3339)?;

    let oracle = match oracles.get(&filters.asset_pair) {
        None => {
            return Ok(make_api_response::<String>(
                None,
                Some(format!("asset pair {} not recorded", filters.asset_pair)),
            ))
        }
        Some(val) => val,
    };

    if oracle.event_database.is_empty() {
        info!("no oracle events found");
        return Err(anyhow!(
            "oracle event with maturation {} not found",
            rfc3339_time
        ));
    }

    info!("retrieving oracle event with maturation {}", rfc3339_time);
    let event = match oracle.event_database.get(rfc3339_time.as_bytes())? {
        Some(val) => val,
        None => {
            return Err(anyhow!(
                "oracle event with maturation {} not found",
                rfc3339_time
            ))
        }
    };
    Ok(make_api_response(
        Some(parse_database_entry(
            filters.asset_pair,
            (rfc3339_time.into(), event),
        )),
        None,
    ))
}

#[get("/announcement/{rfc3339_time}")]
async fn announcement(
    oracles: web::Data<HashMap<AssetPair, Oracle>>,
    filters: web::Query<Filters>,
    path: web::Path<String>,
) -> HttpResponse {
    info!("GET /announcement/{}: {:#?}", path, filters);
    match execute_announcement(&oracles, &filters, &path) {
        Ok(val) => val,
        Err(err) => make_api_response::<String>(None, Some(err.to_string())),
    }
}

#[derive(Parser)]
/// Simple DLC oracle implementation
struct Args {
    /// Optional private key file; if not provided, one is generated
    #[clap(short, long, parse(from_os_str), value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    secret_key_file: Option<std::path::PathBuf>,

    /// Optional asset pair config file; if not provided, it is assumed to exist at "config/asset_pair.json"
    #[clap(short, long, parse(from_os_str), value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    asset_pair_config_file: Option<std::path::PathBuf>,

    /// Optional oracle config file; if not provided, it is assumed to exist at "config/oracle.json"
    #[clap(short, long, parse(from_os_str), value_name = "FILE", value_hint = clap::ValueHint::FilePath)]
    oracle_config_file: Option<std::path::PathBuf>,
}

#[actix_web::main]
async fn main() -> anyhow::Result<()> {
    env_logger::init();

    let args = Args::parse();

    let mut secret_key = String::new();
    let secp = Secp256k1::new();

    let secret_key = match args.secret_key_file {
        None => {
            info!("no secret key file was found, generating secret key");
            secp.generate_keypair(&mut rand::thread_rng()).0
        }
        Some(path) => {
            info!(
                "reading secret key from {}",
                path.as_os_str().to_string_lossy()
            );
            File::open(path)?.read_to_string(&mut secret_key)?;
            secret_key.retain(|c| !c.is_whitespace());
            SecretKey::from_str(&secret_key)?
        }
    };
    let keypair = KeyPair::from_secret_key(&secp, secret_key);
    info!(
        "oracle keypair successfully generated, pubkey is {}",
        keypair.public_key().serialize().encode_hex::<String>()
    );

    let asset_pair_infos: Vec<AssetPairInfo> = match args.asset_pair_config_file {
        None => {
            info!("reading asset pair config from config/asset_pair.json");
            serde_json::from_str(&fs::read_to_string("config/asset_pair.json")?)?
        }
        Some(path) => {
            info!(
                "reading asset pair config from {}",
                path.as_os_str().to_string_lossy()
            );
            let mut asset_pair_info = String::new();
            File::open(path)?.read_to_string(&mut asset_pair_info)?;
            serde_json::from_str(&asset_pair_info)?
        }
    };
    info!(
        "asset pair config successfully read: {:#?}",
        asset_pair_infos
    );

    let oracle_config: OracleConfig = match args.oracle_config_file {
        None => {
            info!("reading oracle config from config/oracle.json");
            serde_json::from_str(&fs::read_to_string("config/oracle.json")?)?
        }
        Some(path) => {
            info!(
                "reading oracle config from {}",
                path.as_os_str().to_string_lossy()
            );
            let mut oracle_config = String::new();
            File::open(path)?.read_to_string(&mut oracle_config)?;
            serde_json::from_str(&oracle_config)?
        }
    };
    info!("oracle config successfully read: {:#?}", oracle_config);

    // setup event databases
    let oracles = asset_pair_infos
        .iter()
        .map(|asset_pair_info| asset_pair_info.asset_pair)
        .zip(asset_pair_infos.iter().cloned().map(|asset_pair_info| {
            let asset_pair = asset_pair_info.asset_pair;

            // create oracle
            info!("creating oracle for {}", asset_pair);
            let oracle = Oracle::new(oracle_config.clone(), asset_pair_info, keypair)?;

            // pricefeed retreival
            info!("creating pricefeeds for {}", asset_pair);
            let pricefeeds: Vec<Box<dyn PriceFeed + Send + Sync>> = vec![
                Box::new(Bitstamp {}),
                Box::new(GateIo {}),
                Box::new(Kraken {}),
            ];

            info!("scheduling oracle events for {}", asset_pair);
            // schedule oracle events (announcements/attestations)
            oracle_scheduler::init(oracle.clone(), secp.clone(), pricefeeds)?;

            Ok(oracle)
        }))
        .map(|(asset_pair, oracle)| oracle.map(|ok| (asset_pair, ok)))
        .collect::<anyhow::Result<HashMap<_, _>>>()?;

    // setup and run server
    info!("starting server");
    HttpServer::new(move || {
        App::new()
            .app_data(web::Data::new(oracles.clone()))
            .service(
                web::scope("/v1")
                    .service(announcements)
                    .service(announcement),
            )
    })
    .bind(("127.0.0.1", 8080))?
    .run()
    .await?;

    Ok(())
}
