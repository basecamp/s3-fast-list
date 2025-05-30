mod error;
mod core;
mod data_map;
mod tasks_s3;
mod utils;
mod stats;
mod mon;
use std::io::BufRead;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use clap::{Parser, Subcommand};
use chrono::{Local, SecondsFormat};
use log::info;
use core::MB;
use core::RunMode;

#[derive(Parser)]
#[command(author, version, about, long_about = None)]
#[command(propagate_version = true)]
struct Cli {
    #[command(subcommand)]
    cmd: Commands,

    /// prefix to start with
    #[arg(short, long, default_value = "/", global=true)]
    prefix: String,

    /// worker threads for runtime
    #[arg(short, long, default_value_t = 10, global=true)]
    threads: usize,

    /// max concurrency tasks for list operation
    #[arg(short, long, default_value_t = 100, global=true)]
    concurrency: usize,

    /// input key space hints file [default: {region}_{bucket}_ks_hints.input]
    #[arg(short, long, global=true)]
    ks_file: Option<String>,

    /// object filter expresion
    #[arg(short, long, global=true)]
    filter: Option<String>,

    /// log to file [default: fastlist_{datetime}.log]
    #[arg(short, long, global=true)]
    log: bool,

    /// custom S3 endpoint URL
    #[arg(long = "endpoint-url", global=true)]
    endpoint: Option<String>,

    /// force path-style addressing (default when using --endpoint-url)
    #[arg(long, global=true)]
    force_path_style: bool,

    /// log file path (implies --log) [default: fastlist_{datetime}.log]
    #[arg(long, global=true)]
    output_log_file: Option<String>,

    /// keyspace file output path [default: {region}_{bucket}_{datetime}.ks]
    #[arg(long, global=true)]
    output_ks_file: Option<String>,

    /// parquet file output path [default: {region}_{bucket}_{datetime}.parquet]
    #[arg(long, global=true)]
    output_parquet_file: Option<String>,
}

#[derive(Subcommand)]
enum Commands {

    /// fast list and export results
    List {
        /// source aws region (optional, uses AWS SDK defaults if not provided)
        #[arg(long)]
        region: Option<String>,

        /// source bucket to list
        #[arg(long)]
        bucket: String,
    },

    /// bi-dir fast list and diff results
    Diff {
        /// source aws region (optional, uses AWS SDK defaults if not provided)
        #[arg(long)]
        region: Option<String>,

        /// source bucket to list
        #[arg(long)]
        bucket: String,

        /// target aws region (optional, uses AWS SDK defaults if not provided)
        #[arg(long)]
        target_region: Option<String>,

        /// target bucket to list
        #[arg(long)]
        target_bucket: String,
    },
}

fn main() {

    let cli = Cli::parse();
    let opt_mode;
    let opt_region;
    let opt_bucket;
    let opt_target_region;
    let opt_target_bucket;
    let opt_prefix = if cli.prefix == "/" { "".to_string() } else { cli.prefix };
    let opt_threads = cli.threads;
    let opt_concurrency = cli.concurrency;
    let opt_filter = cli.filter;

    // baseline count for all main tasks
    // data map task and mon task
    let mut g_tasks_count = 2;

    match &cli.cmd {
        Commands::List { region, bucket } => {
            opt_mode = RunMode::List;
            opt_region = region;
            opt_bucket = bucket;
            opt_target_region = None;
            opt_target_bucket = None;
            g_tasks_count += 1;
        },
        Commands::Diff { region, bucket, target_region, target_bucket } => {
            opt_mode = RunMode::BiDir;
            opt_region = region;
            opt_bucket = bucket;
            opt_target_region = Some(target_region);
            opt_target_bucket = Some(target_bucket);
            g_tasks_count += 2;
        },
    }

    // extract endpoint and path style options
    let opt_endpoint = cli.endpoint;

    // Use path-style addressing if explicitly requested or if a custom endpoint is provided
    let opt_force_path_style = cli.force_path_style || opt_endpoint.is_some();

    // Extract output file options
    let opt_output_ks_file = cli.output_ks_file;
    let opt_output_parquet_file = cli.output_parquet_file;
    let opt_output_log_file = cli.output_log_file;

    // setup loglevel and log file
    // if output_log_file is set, it implies log=true
    let opt_log = cli.log || opt_output_log_file.is_some();
    let package_name = env!("CARGO_PKG_NAME").replace("-", "_");
    let loglevel_s = format!("{}=info", package_name);
    let loglevel = std::env::var("RUST_LOG").unwrap_or(loglevel_s);

    if opt_log {
        // Use specified log file path or generate default with timestamp
        let logfile_s = match &opt_output_log_file {
            Some(path) => path.clone(),
            None => format!("fastlist_{}.log", Local::now().format("%Y%m%d%H%M%S"))
        };

        let logfile = std::fs::OpenOptions::new()
                                .write(true)
                                .create(true)
                                .append(true)
                                .open(&logfile_s)
                                .expect("unable to open log file");
        env_logger::Builder::new()
            .parse_filters(&loglevel)
            .target(env_logger::Target::Pipe(Box::new(logfile)))
            .init();
    } else {
        env_logger::Builder::new()
            .parse_filters(&loglevel)
            .init();
    }

    // gen dt string
    let dt_str = Local::now().to_rfc3339_opts(SecondsFormat::Secs, true);

    // prepare ks hints list
    let mut ks_list: Vec::<String> = Vec::new();

    // check ks hints from cli input
    let opt_ks_file = cli.ks_file;
    let ks_filename = if let Some(f) = opt_ks_file {
        f.to_string()
    } else {
        // default ks hints input filename - include region if provided
        if let Some(region) = &opt_region {
            format!("{}_{}_{}", region, opt_bucket, "ks_hints.input")
        } else {
            format!("{}_{}", opt_bucket, "ks_hints.input")
        }
    };

    // load ks hints if exists
    if let Ok(f) = std::fs::File::open(&ks_filename) {
        let lines = std::io::BufReader::with_capacity(50*MB, f).lines();
        ks_list = lines.map(|l| l.unwrap()).collect();
    }

    // sort input lexicographically
    ks_list.sort();
    // dedup
    ks_list.dedup();
    let ks_list_len = ks_list.len();

    let ks_hints = data_map::KeySpaceHints::new_from(&ks_list);
    let ks_hints_pairs_len = ks_hints.len();

    info!("fast list tools v{} starting:", env!("CARGO_PKG_VERSION"));
    info!("  - mode {:?}, threads {}, concurrent tasks {}", opt_mode, opt_threads, opt_concurrency);
    info!("  - start prefix {}", opt_prefix);
    if let Some(region) = &opt_region {
        info!("  - region {}", region);
    }
    if opt_filter.is_some() {
        info!("  - filter \"{}\"", opt_filter.as_ref().unwrap());
    }
    if let Some(endpoint) = &opt_endpoint {
        info!("  - using custom endpoint-url: {}", endpoint);
    }
    if opt_force_path_style {
        info!("  - using path-style addressing");
    }
    if ks_list_len == 0 {
        info!("  - NO ks hints found");
    } else {
        info!("  - loaded {} prefix from input file {}, assembly into {} of ks hints pairs", ks_list_len, ks_filename, ks_hints_pairs_len);
    }

    let quit = Arc::new(AtomicBool::new(false));
    let q = quit.clone();
    ctrlc::set_handler(move || {
        q.store(true, Ordering::SeqCst);
    }).expect("failed to setting ctrl-c signal handler");

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(opt_threads)
        .build()
        .unwrap();

    rt.block_on(async {
        let g_state = core::GlobalState::new(quit, g_tasks_count, 0);
        let mut set = tokio::task::JoinSet::new();

        let (data_map_channel, data_map_channel_rx) = tokio::sync::mpsc::unbounded_channel();

        // init left task
        let prefix = opt_prefix.clone();
        let dir = if opt_mode == RunMode::BiDir {
            core::S3_TASK_CONTEXT_DIR_LEFT_DIFF_MODE
        } else {
            core::S3_TASK_CONTEXT_DIR_LEFT_LIST_MODE
        };
        let task_ctx = core::S3TaskContext::new(opt_bucket,
            opt_region.as_ref().map(|s| s.as_str()), opt_endpoint.as_ref().map(|s| s.as_str()),
            opt_force_path_style, data_map_channel.clone(), dir, g_state.clone()
        );
        set.spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async move {
                tasks_s3::flat_list_main_task(&task_ctx, &prefix, opt_concurrency, ks_hints).await
            })
        });

        // init right task if bidir mode
        if opt_mode == RunMode::BiDir {
            let prefix = opt_prefix.clone();
            // Extract target_region from double-wrapped option
            let target_region_str = match opt_target_region.as_ref() {
                Some(inner_opt) => inner_opt.as_ref().map(|s| s.as_str()),
                None => None,
            };

            let task_ctx = core::S3TaskContext::new(opt_target_bucket.as_ref().unwrap(),
                target_region_str, opt_endpoint.as_ref().map(|s| s.as_str()), opt_force_path_style,
                data_map_channel, core::S3_TASK_CONTEXT_DIR_RIGHT_DIFF_MODE, g_state.clone()
            );
            let ks_hints = data_map::KeySpaceHints::new_from(&ks_list);
            set.spawn_blocking(move || {
                tokio::runtime::Handle::current().block_on(async move {
                    tasks_s3::flat_list_main_task(&task_ctx, &prefix, opt_concurrency, ks_hints).await
                })
            });
        }

        // init data map task
        let data_map_ctx = core::DataMapContext::new(data_map_channel_rx, g_state.clone(), opt_filter, opt_mode.clone());

        // Generate output filenames with region if provided
        let region_prefix = if let Some(region) = opt_region {
            format!("{}_", region)
        } else {
            "".to_string()
        };

        // Use custom KS file path if provided, otherwise generate default
        let filename_ks = match &opt_output_ks_file {
            Some(path) => path.clone(),
            None => format!("{}_{}_{}.ks", region_prefix, opt_bucket, dt_str)
        };

        // Use custom parquet file path if provided, otherwise generate default
        let filename_output = match &opt_output_parquet_file {
            Some(path) => path.clone(),
            None => {
                if opt_mode == RunMode::List {
                    format!("{}_{}_{}.parquet", region_prefix, opt_bucket, dt_str)
                } else {
                    let target_region_prefix = if let Some(Some(target_region)) = &opt_target_region {
                        format!("{}_", target_region)
                    } else {
                        "".to_string()
                    };
                    format!("{}_{}_{}_{}_{}.parquet", region_prefix, opt_bucket,
                        target_region_prefix, opt_target_bucket.as_ref().unwrap(), dt_str)
                }
            }
        };
        set.spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async move {
                data_map::data_map_task(data_map_ctx, filename_ks, filename_output).await
            })
        });

        // init mon task
        let mon_ctx = core::MonContext::new(g_state.clone());
        set.spawn_blocking(move || {
            tokio::runtime::Handle::current().block_on(async move {
                mon::mon_task(mon_ctx).await
            })
        });

        while let Some(_) = set.join_next().await {
        }
        info!("All Tasks quit");
    });

    rt.shutdown_background();
}
