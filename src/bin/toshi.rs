use std::net::SocketAddr;
use std::{
    fs::create_dir,
    path::{Path, PathBuf},
    sync::{Arc, RwLock},
};

use clap::{crate_authors, crate_description, crate_version, App, Arg, ArgMatches};
use futures::{future, sync::oneshot, Future, Stream};
use log::{error, info};
use tokio::runtime::Runtime;

use toshi::{
    cluster::{self, rpc_server::RpcServer, Consul},
    commit::IndexWatcher,
    index::IndexCatalog,
    router::router_with_catalog,
    settings::{Settings, HEADER, RPC_HEADER},
};

pub fn main() -> Result<(), ()> {
    let settings = settings();

    std::env::set_var("RUST_LOG", &settings.log_level);
    pretty_env_logger::init();
    info!("{:?}", &settings);

    let mut rt = Runtime::new().expect("failed to start new Runtime");

    let (tx, shutdown_signal) = oneshot::channel();

    if !Path::new(&settings.path).exists() {
        info!("Base data path {} does not exist, creating it...", settings.path);
        create_dir(settings.path.clone()).expect("Unable to create data directory");
    }

    let index_catalog = {
        let path = PathBuf::from(settings.path.clone());
        let index_catalog = match IndexCatalog::new(path, settings.clone()) {
            Ok(v) => v,
            Err(e) => {
                eprintln!("Error creating IndexCatalog from path {} - {}", settings.path, e);
                std::process::exit(1);
            }
        };

        Arc::new(RwLock::new(index_catalog))
    };

    let toshi = {
        let server = if settings.master {
            future::Either::A(run(index_catalog.clone(), &settings))
        } else {
            let addr = format!("{}:{}", &settings.host, settings.port);
            println!("{}", RPC_HEADER);
            info!("I am a data node...Binding to: {}", addr);
            let bind: SocketAddr = addr.parse().unwrap();
            future::Either::B(RpcServer::get_service(bind, Arc::clone(&index_catalog)))
        };
        let shutdown = shutdown(tx);
        server.select(shutdown)
    };

    rt.spawn(toshi.map(|_| ()).map_err(|_| ()));

    shutdown_signal
        .map_err(|_| unreachable!("Shutdown signal channel should not error, This is a bug."))
        .and_then(move |_| {
            index_catalog
                .write()
                .expect("Unable to acquire write lock on index catalog")
                .clear();
            Ok(())
        })
        .and_then(move |_| rt.shutdown_now())
        .wait()
}

fn settings() -> Settings {
    let options: ArgMatches = App::new("Toshi Search")
        .version(crate_version!())
        .about(crate_description!())
        .author(crate_authors!())
        .arg(
            Arg::with_name("config")
                .short("c")
                .long("config")
                .takes_value(true)
                .default_value("config/config.toml"),
        )
        .arg(
            Arg::with_name("level")
                .short("l")
                .long("level")
                .takes_value(true)
                .default_value("info"),
        )
        .arg(
            Arg::with_name("path")
                .short("d")
                .long("data-path")
                .takes_value(true)
                .default_value("data/"),
        )
        .arg(
            Arg::with_name("host")
                .short("h")
                .long("host")
                .takes_value(true)
                .default_value("0.0.0.0"),
        )
        .arg(
            Arg::with_name("port")
                .short("p")
                .long("port")
                .takes_value(true)
                .default_value("8080"),
        )
        .arg(
            Arg::with_name("consul-addr")
                .short("C")
                .long("consul-addr")
                .takes_value(true)
                .default_value("127.0.0.1:8500"),
        )
        .arg(
            Arg::with_name("cluster-name")
                .short("N")
                .long("cluster-name")
                .takes_value(true)
                .default_value("kitsune"),
        )
        .arg(
            Arg::with_name("enable-clustering")
                .short("e")
                .long("enable-clustering")
                .takes_value(true),
        )
        .get_matches();

    match options.value_of("config") {
        Some(v) => Settings::new(v).expect("Invalid configuration file"),
        None => Settings::from_args(&options),
    }
}

fn run(catalog: Arc<RwLock<IndexCatalog>>, settings: &Settings) -> impl Future<Item = (), Error = ()> {
    let commit_watcher = if settings.auto_commit_duration > 0 {
        let commit_watcher = IndexWatcher::new(catalog.clone(), settings.auto_commit_duration);
        future::Either::A(future::lazy(move || {
            commit_watcher.start();
            future::ok::<(), ()>(())
        }))
    } else {
        future::Either::B(future::ok::<(), ()>(()))
    };

    let addr = format!("{}:{}", &settings.host, settings.port);
    let bind: SocketAddr = addr.parse().expect("Failed to parse socket address");

    println!("{}", HEADER);

    if settings.enable_clustering {
        let settings = settings.clone();
        let place_addr = settings.place_addr.clone();
        let consul_addr = settings.consul_addr.clone();
        let cluster_name = settings.cluster_name.clone();

        let run = future::lazy(move || connect_to_consul(&settings)).and_then(move |_| {
            tokio::spawn(commit_watcher);

            let mut consul = Consul::builder()
                .with_cluster_name(cluster_name)
                .with_address(consul_addr)
                .build()
                .expect("Could not build Consul client.");

            let place_addr = place_addr.parse().expect("Placement address must be a valid SocketAddr");
            tokio::spawn(cluster::run(place_addr, consul).map_err(|e| error!("Error with running cluster: {}", e)));

            router_with_catalog(&bind, &catalog)
        });

        future::Either::A(run)
    } else {
        let run = commit_watcher.and_then(move |_| router_with_catalog(&bind, &catalog));
        future::Either::B(run)
    }
}

fn connect_to_consul(settings: &Settings) -> impl Future<Item = (), Error = ()> {
    let consul_address = settings.consul_addr.clone();
    let cluster_name = settings.cluster_name.clone();
    let settings_path = settings.path.clone();

    future::lazy(move || {
        let mut consul_client = Consul::builder()
            .with_cluster_name(cluster_name)
            .with_address(consul_address)
            .build()
            .expect("Could not build Consul client.");

        // Build future that will connect to Consul and register the node_id
        consul_client
            .register_cluster()
            .and_then(|_| cluster::init_node_id(settings_path))
            .and_then(move |id| {
                consul_client.set_node_id(id);
                consul_client.register_node()
            })
            .map_err(|e| error!("Error: {}", e))
    })
}

#[cfg(unix)]
fn shutdown(signal: oneshot::Sender<()>) -> impl Future<Item = (), Error = ()> {
    use tokio_signal::unix::{Signal, SIGINT, SIGTERM};

    let sigint = Signal::new(SIGINT).flatten_stream().map(|_| String::from("SIGINT"));
    let sigterm = Signal::new(SIGTERM).flatten_stream().map(|_| String::from("SIGTERM"));

    handle_shutdown(signal, sigint.select(sigterm))
}
#[cfg(not(unix))]
fn shutdown(signal: oneshot::Sender<()>) -> impl Future<Item = (), Error = ()> {
    let stream = tokio_signal::ctrl_c().flatten_stream().map(|_| String::from("ctrl-r"));
    handle_shutdown(signal, stream)
}

fn handle_shutdown<S>(signal: oneshot::Sender<()>, stream: S) -> impl Future<Item = (), Error = ()>
where
    S: Stream<Item = String, Error = std::io::Error>,
{
    stream
        .take(1)
        .into_future()
        .and_then(move |(sig, _)| {
            if let Some(s) = sig {
                info!("Received signal: {}", s);
            }
            info!("Gracefully shutting down...");
            Ok(signal.send(()))
        })
        .map(|_| ())
        .map_err(|_| unreachable!("Signal handling should never error out"))
}
