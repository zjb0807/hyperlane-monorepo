#![allow(clippy::doc_lazy_continuation)] // TODO: `rustc` 1.80.1 clippy issue

//! Run this from the hyperlane-monorepo/rust directory using `cargo run -r -p
//! run-locally`.
//!
//! Environment arguments:
//! - `E2E_CI_MODE`: true/false, enables CI mode which will automatically wait
//!   for kathy to finish
//! running and for the queues to empty. Defaults to false.
//! - `E2E_CI_TIMEOUT_SEC`: How long (in seconds) to allow the main loop to run
//!   the test for. This
//! does not include the initial setup time. If this timeout is reached before
//! the end conditions are met, the test is a failure. Defaults to 10 min.
//! - `E2E_KATHY_MESSAGES`: Number of kathy messages to dispatch. Defaults to 16 if CI mode is enabled.
//! else false.
//! - `SEALEVEL_ENABLED`: true/false, enables sealevel testing. Defaults to true.

use std::{
    collections::HashMap,
    fs::{self, File},
    path::Path,
    process::{Child, ExitCode},
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc, Mutex,
    },
    thread::sleep,
    time::{Duration, Instant},
};

use ethers_contract::MULTICALL_ADDRESS;
use logging::log;
pub use metrics::fetch_metric;
use once_cell::sync::Lazy;
use program::Program;
use tempfile::tempdir;

use crate::{
    config::Config,
    ethereum::start_anvil,
    invariants::{post_startup_invariants, termination_invariants_met, SOL_MESSAGES_EXPECTED},
    metrics::agent_balance_sum,
    solana::*,
    utils::{concat_path, make_static, stop_child, AgentHandles, ArbitraryData, TaskHandle},
};

mod config;
mod cosmos;
mod ethereum;
mod invariants;
mod logging;
mod metrics;
mod program;
mod server;
mod solana;
mod utils;

pub static AGENT_LOGGING_DIR: Lazy<&Path> = Lazy::new(|| {
    let dir = Path::new("/tmp/test_logs");
    fs::create_dir_all(dir).unwrap();
    dir
});

/// These private keys are from hardhat/anvil's testing accounts.
const RELAYER_KEYS: &[&str] = &[
    // test1
    "0x2a871d0798f97d79848a013d4936a73bf4cc922c825d33c1cf7073dff6d409c6",
    // test2
    "0xdbda1821b80551c9d65939329250298aa3472ba22feea921c0cf5d620ea67b97",
    // test3
    "0x4bbbf85ce3377467afe5d46f804f221813b2bb87f24d81f60f1fcdbf7cbf4356",
    // sealeveltest1
    "0x892bf6949af4233e62f854cb3618bc1a3ee3341dc71ada08c4d5deca239acf4f",
    // sealeveltest2
    "0x892bf6949af4233e62f854cb3618bc1a3ee3341dc71ada08c4d5deca239acf4f",
];
/// These private keys are from hardhat/anvil's testing accounts.
/// These must be consistent with the ISM config for the test.
const ETH_VALIDATOR_KEYS: &[&str] = &[
    // eth
    "0x47e179ec197488593b187f80a00eb0da91f1b9d0b13f8733639f19c30a34926a",
    "0x8b3a350cf5c34c9194ca85829a2df0ec3153be0318b5e2d3348e872092edffba",
    "0x92db14e403b83dfe3df233f83dfa3a0d7096f21ca9b0d6d6b8d88b2b4ec1564e",
];

const SEALEVEL_VALIDATOR_KEYS: &[&str] = &[
    // sealevel
    "0x59c6995e998f97a5a0044966f0945389dc9e86dae88c7a8412f4603b6b78690d",
];

const AGENT_BIN_PATH: &str = "target/debug";
const SOLANA_AGNET_BIN_PATH: &str = "../sealevel/target/debug/";
const INFRA_PATH: &str = "../../typescript/infra";
const MONOREPO_ROOT_PATH: &str = "../../";

const ZERO_MERKLE_INSERTION_KATHY_MESSAGES: u32 = 600;

const RELAYER_METRICS_PORT: &str = "9092";
const SCRAPER_METRICS_PORT: &str = "9093";

type DynPath = Box<dyn AsRef<Path>>;

static RUN_LOG_WATCHERS: AtomicBool = AtomicBool::new(true);
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Struct to hold stuff we want to cleanup whenever we exit. Just using for
/// cleanup purposes at this time.
#[derive(Default)]
struct State {
    #[allow(clippy::type_complexity)]
    agents: HashMap<String, (Child, Option<Arc<Mutex<File>>>)>,
    watchers: Vec<Box<dyn TaskHandle<Output = ()>>>,
    data: Vec<Box<dyn ArbitraryData>>,
}

impl State {
    fn push_agent(&mut self, handles: AgentHandles) {
        self.agents.insert(handles.0, (handles.1, handles.5));
        self.watchers.push(handles.2);
        self.watchers.push(handles.3);
        self.data.push(handles.4);
    }
}

impl Drop for State {
    fn drop(&mut self) {
        SHUTDOWN.store(true, Ordering::Relaxed);
        log!("Signaling children to stop...");
        for (name, (mut agent, _)) in self.agents.drain() {
            log!("Stopping child {}", name);
            stop_child(&mut agent);
        }
        log!("Joining watchers...");
        RUN_LOG_WATCHERS.store(false, Ordering::Relaxed);
        for w in self.watchers.drain(..) {
            w.join_box();
        }
        // drop any held data
        self.data.reverse();
        for data in self.data.drain(..) {
            drop(data)
        }
        fs::remove_dir_all(SOLANA_CHECKPOINT_LOCATION).unwrap_or_default();
        fs::remove_dir_all::<&Path>(AGENT_LOGGING_DIR.as_ref()).unwrap_or_default();
    }
}

fn main() -> ExitCode {
    // on sigint we want to trigger things to stop running
    ctrlc::set_handler(|| {
        log!("Terminating...");
        SHUTDOWN.store(true, Ordering::Relaxed);
    })
    .unwrap();

    let config = Config::load();
    log!("Running with config: {:?}", config);

    let mut validator_origin_chains = ["test1", "test2", "test3"].to_vec();
    let mut validator_keys = ETH_VALIDATOR_KEYS.to_vec();
    let mut validator_count: usize = validator_keys.len();
    let mut checkpoints_dirs: Vec<DynPath> = (0..validator_count)
        .map(|_| Box::new(tempdir().unwrap()) as DynPath)
        .collect();
    if config.sealevel_enabled {
        validator_origin_chains.push("sealeveltest1");
        let mut sealevel_keys = SEALEVEL_VALIDATOR_KEYS.to_vec();
        validator_keys.append(&mut sealevel_keys);
        let solana_checkpoint_path = Path::new(SOLANA_CHECKPOINT_LOCATION);
        fs::remove_dir_all(solana_checkpoint_path).unwrap_or_default();
        checkpoints_dirs.push(Box::new(solana_checkpoint_path) as DynPath);
        validator_count += 1;
    }
    assert_eq!(validator_origin_chains.len(), validator_keys.len());

    let rocks_db_dir = tempdir().unwrap();
    let relayer_db = concat_path(&rocks_db_dir, "relayer");
    let validator_dbs = (0..validator_count)
        .map(|i| concat_path(&rocks_db_dir, format!("validator{i}")))
        .collect::<Vec<_>>();

    let common_agent_env = Program::default()
        .env("RUST_BACKTRACE", "full")
        .hyp_env("LOG_FORMAT", "compact")
        .hyp_env("LOG_LEVEL", "debug")
        .hyp_env("CHAINS_TEST1_INDEX_CHUNK", "1")
        .hyp_env("CHAINS_TEST2_INDEX_CHUNK", "1")
        .hyp_env("CHAINS_TEST3_INDEX_CHUNK", "1");

    let multicall_address_string: String = format!("0x{}", hex::encode(MULTICALL_ADDRESS));

    let relayer_env = common_agent_env
        .clone()
        .bin(concat_path(AGENT_BIN_PATH, "relayer"))
        .hyp_env("CHAINS_TEST1_RPCCONSENSUSTYPE", "fallback")
        .hyp_env(
            "CHAINS_TEST2_CONNECTION_URLS",
            "http://127.0.0.1:8545,http://127.0.0.1:8545,http://127.0.0.1:8545",
        )
        .hyp_env(
            "CHAINS_TEST1_BATCHCONTRACTADDRESS",
            multicall_address_string.clone(),
        )
        .hyp_env("CHAINS_TEST1_MAXBATCHSIZE", "5")
        // by setting this as a quorum provider we will cause nonce errors when delivering to test2
        // because the message will be sent to the node 3 times.
        .hyp_env("CHAINS_TEST2_RPCCONSENSUSTYPE", "quorum")
        .hyp_env(
            "CHAINS_TEST2_BATCHCONTRACTADDRESS",
            multicall_address_string.clone(),
        )
        .hyp_env("CHAINS_TEST2_MAXBATCHSIZE", "5")
        .hyp_env("CHAINS_TEST3_CONNECTION_URL", "http://127.0.0.1:8545")
        .hyp_env(
            "CHAINS_TEST3_BATCHCONTRACTADDRESS",
            multicall_address_string,
        )
        .hyp_env("CHAINS_TEST3_MAXBATCHSIZE", "5")
        .hyp_env("METRICSPORT", RELAYER_METRICS_PORT)
        .hyp_env("DB", relayer_db.to_str().unwrap())
        .hyp_env("CHAINS_TEST1_SIGNER_KEY", RELAYER_KEYS[0])
        .hyp_env("CHAINS_TEST2_SIGNER_KEY", RELAYER_KEYS[1])
        .hyp_env("CHAINS_SEALEVELTEST1_SIGNER_KEY", RELAYER_KEYS[3])
        .hyp_env("CHAINS_SEALEVELTEST2_SIGNER_KEY", RELAYER_KEYS[4])
        .hyp_env("RELAYCHAINS", "invalidchain,otherinvalid")
        .hyp_env("ALLOWLOCALCHECKPOINTSYNCERS", "true")
        .hyp_env(
            "GASPAYMENTENFORCEMENT",
            r#"[{
                "type": "minimum",
                "payment": "1",
            }]"#,
        )
        .arg(
            "chains.test1.customRpcUrls",
            "http://127.0.0.1:8545,http://127.0.0.1:8545,http://127.0.0.1:8545",
        )
        // default is used for TEST3
        .arg("defaultSigner.key", RELAYER_KEYS[2]);
    let relayer_env = if config.sealevel_enabled {
        relayer_env.arg(
            "relayChains",
            "test1,test2,test3,sealeveltest1,sealeveltest2",
        )
    } else {
        relayer_env.arg("relayChains", "test1,test2,test3")
    };

    let base_validator_env = common_agent_env
        .clone()
        .bin(concat_path(AGENT_BIN_PATH, "validator"))
        .hyp_env(
            "CHAINS_TEST1_CUSTOMRPCURLS",
            "http://127.0.0.1:8545,http://127.0.0.1:8545,http://127.0.0.1:8545",
        )
        .hyp_env("CHAINS_TEST1_RPCCONSENSUSTYPE", "quorum")
        .hyp_env(
            "CHAINS_TEST2_CUSTOMRPCURLS",
            "http://127.0.0.1:8545,http://127.0.0.1:8545,http://127.0.0.1:8545",
        )
        .hyp_env("CHAINS_TEST2_RPCCONSENSUSTYPE", "fallback")
        .hyp_env("CHAINS_TEST3_CUSTOMRPCURLS", "http://127.0.0.1:8545")
        .hyp_env("CHAINS_TEST1_BLOCKS_REORGPERIOD", "0")
        .hyp_env("CHAINS_TEST2_BLOCKS_REORGPERIOD", "0")
        .hyp_env("CHAINS_TEST3_BLOCKS_REORGPERIOD", "0")
        .hyp_env("INTERVAL", "5")
        .hyp_env("CHECKPOINTSYNCER_TYPE", "localStorage");

    let validator_envs = (0..validator_count)
        .map(|i| {
            base_validator_env
                .clone()
                .hyp_env("METRICSPORT", (9094 + i).to_string())
                .hyp_env("DB", validator_dbs[i].to_str().unwrap())
                .hyp_env("ORIGINCHAINNAME", validator_origin_chains[i])
                .hyp_env("VALIDATOR_KEY", validator_keys[i])
                .hyp_env(
                    "CHECKPOINTSYNCER_PATH",
                    (*checkpoints_dirs[i]).as_ref().to_str().unwrap(),
                )
        })
        .collect::<Vec<_>>();

    let scraper_env = common_agent_env
        .bin(concat_path(AGENT_BIN_PATH, "scraper"))
        .hyp_env("CHAINS_TEST1_RPCCONSENSUSTYPE", "quorum")
        .hyp_env("CHAINS_TEST1_CUSTOMRPCURLS", "http://127.0.0.1:8545")
        .hyp_env("CHAINS_TEST2_RPCCONSENSUSTYPE", "quorum")
        .hyp_env("CHAINS_TEST2_CUSTOMRPCURLS", "http://127.0.0.1:8545")
        .hyp_env("CHAINS_TEST3_RPCCONSENSUSTYPE", "quorum")
        .hyp_env("CHAINS_TEST3_CUSTOMRPCURLS", "http://127.0.0.1:8545")
        .hyp_env("METRICSPORT", SCRAPER_METRICS_PORT)
        .hyp_env(
            "DB",
            "postgresql://postgres:47221c18c610@localhost:5432/postgres",
        );
    let scraper_env = if config.sealevel_enabled {
        scraper_env.hyp_env(
            "CHAINSTOSCRAPE",
            "test1,test2,test3,sealeveltest1,sealeveltest2",
        )
    } else {
        scraper_env.hyp_env("CHAINSTOSCRAPE", "test1,test2,test3")
    };

    let mut state = State::default();

    log!(
        "Signed checkpoints in {}",
        checkpoints_dirs
            .iter()
            .map(|d| (**d).as_ref().display().to_string())
            .collect::<Vec<_>>()
            .join(", ")
    );
    log!("Relayer DB in {}", relayer_db.display());
    (0..validator_count).for_each(|i| {
        log!("Validator {} DB in {}", i + 1, validator_dbs[i].display());
    });

    //
    // Ready to run...
    //

    let solana_paths = if config.sealevel_enabled {
        let (solana_path, solana_path_tempdir) = install_solana_cli_tools(
            SOLANA_CONTRACTS_CLI_RELEASE_URL.to_owned(),
            SOLANA_CONTRACTS_CLI_VERSION.to_owned(),
        )
        .join();
        state.data.push(Box::new(solana_path_tempdir));
        let solana_program_builder = build_solana_programs(solana_path.clone());
        Some((solana_program_builder.join(), solana_path))
    } else {
        None
    };

    // this task takes a long time in the CI so run it in parallel
    log!("Building rust...");
    let build_main = Program::new("cargo")
        .cmd("build")
        .arg("features", "test-utils memory-profiling")
        .arg("bin", "relayer")
        .arg("bin", "validator")
        .arg("bin", "scraper")
        .arg("bin", "init-db")
        .filter_logs(|l| !l.contains("workspace-inheritance"))
        .run();

    let start_anvil = start_anvil(config.clone());

    log!("Running postgres db...");
    let postgres = Program::new("docker")
        .cmd("run")
        .flag("rm")
        .arg("name", "scraper-testnet-postgres")
        .arg("env", "POSTGRES_PASSWORD=47221c18c610")
        .arg("publish", "5432:5432")
        .cmd("postgres:14")
        .spawn("SQL", None);
    state.push_agent(postgres);

    build_main.join();
    if config.sealevel_enabled {
        Program::new("cargo")
            .working_dir("../sealevel")
            .cmd("build")
            .arg("bin", "hyperlane-sealevel-client")
            .filter_logs(|l| !l.contains("workspace-inheritance"))
            .run()
            .join();
    }

    let solana_ledger_dir = tempdir().unwrap();
    let solana_config_path = if let Some((solana_program_path, _)) = solana_paths.clone() {
        // use the agave 2.x validator version to ensure mainnet compatibility
        let (solana_path, solana_path_tempdir) = install_solana_cli_tools(
            SOLANA_NETWORK_CLI_RELEASE_URL.to_owned(),
            SOLANA_NETWORK_CLI_VERSION.to_owned(),
        )
        .join();
        state.data.push(Box::new(solana_path_tempdir));
        let start_solana_validator = start_solana_test_validator(
            solana_path.clone(),
            solana_program_path,
            solana_ledger_dir.as_ref().to_path_buf(),
        );

        let (solana_config_path, solana_validator) = start_solana_validator.join();
        state.push_agent(solana_validator);
        Some(solana_config_path)
    } else {
        None
    };

    state.push_agent(start_anvil.join());

    // spawn 1st validator before any messages have been sent to test empty mailbox
    state.push_agent(validator_envs.first().unwrap().clone().spawn("VL1", None));

    sleep(Duration::from_secs(5));

    log!("Init postgres db...");
    Program::new(concat_path(AGENT_BIN_PATH, "init-db"))
        .run()
        .join();
    state.push_agent(scraper_env.spawn("SCR", None));

    // Send half the kathy messages before starting the rest of the agents
    let kathy_env_single_insertion = Program::new("yarn")
        .working_dir(INFRA_PATH)
        .cmd("kathy")
        .arg("messages", (config.kathy_messages / 4).to_string())
        .arg("timeout", "1000");
    kathy_env_single_insertion.clone().run().join();

    let kathy_env_zero_insertion = Program::new("yarn")
        .working_dir(INFRA_PATH)
        .cmd("kathy")
        .arg(
            "messages",
            (ZERO_MERKLE_INSERTION_KATHY_MESSAGES / 2).to_string(),
        )
        .arg("timeout", "1000")
        // replacing the `aggregationHook` with the `interchainGasPaymaster` means there
        // is no more `merkleTreeHook`, causing zero merkle insertions to occur.
        .arg("default-hook", "interchainGasPaymaster");
    kathy_env_zero_insertion.clone().run().join();

    let kathy_env_double_insertion = Program::new("yarn")
        .working_dir(INFRA_PATH)
        .cmd("kathy")
        .arg("messages", (config.kathy_messages / 4).to_string())
        .arg("timeout", "1000")
        // replacing the `protocolFees` required hook with the `merkleTreeHook`
        // will cause double insertions to occur, which should be handled correctly
        .arg("required-hook", "merkleTreeHook");
    kathy_env_double_insertion.clone().run().join();

    if let Some((solana_config_path, (_, solana_path))) =
        solana_config_path.clone().zip(solana_paths.clone())
    {
        // Send some sealevel messages before spinning up the agents, to test the backward indexing cursor
        for _i in 0..(SOL_MESSAGES_EXPECTED / 2) {
            initiate_solana_hyperlane_transfer(solana_path.clone(), solana_config_path.clone())
                .join();
        }
    }

    // spawn the rest of the validators
    for (i, validator_env) in validator_envs.into_iter().enumerate().skip(1) {
        let validator = validator_env.spawn(
            make_static(format!("VL{}", 1 + i)),
            Some(AGENT_LOGGING_DIR.as_ref()),
        );
        state.push_agent(validator);
    }

    state.push_agent(relayer_env.spawn("RLY", Some(&AGENT_LOGGING_DIR)));

    if let Some((solana_config_path, (_, solana_path))) =
        solana_config_path.clone().zip(solana_paths.clone())
    {
        // Send some sealevel messages before spinning up the agents, to test the backward indexing cursor
        for _i in 0..(SOL_MESSAGES_EXPECTED / 2) {
            initiate_solana_hyperlane_transfer(solana_path.clone(), solana_config_path.clone())
                .join();
        }
    }

    log!("Setup complete! Agents running in background...");
    log!("Ctrl+C to end execution...");

    // Send half the kathy messages after the relayer comes up
    kathy_env_double_insertion.clone().run().join();
    kathy_env_zero_insertion.clone().run().join();
    state.push_agent(
        kathy_env_single_insertion
            .flag("mineforever")
            .spawn("KTY", None),
    );

    let loop_start = Instant::now();
    // give things a chance to fully start.
    sleep(Duration::from_secs(10));

    if !post_startup_invariants(&checkpoints_dirs) {
        log!("Failure: Post startup invariants are not met");
        return report_test_result(true);
    } else {
        log!("Success: Post startup invariants are met");
    }

    let mut failure_occurred = false;
    let starting_relayer_balance: f64 = agent_balance_sum(9092).unwrap();
    while !SHUTDOWN.load(Ordering::Relaxed) {
        if config.ci_mode {
            // for CI we have to look for the end condition.
            if termination_invariants_met(
                &config,
                starting_relayer_balance,
                solana_paths
                    .clone()
                    .map(|(_, solana_path)| solana_path)
                    .as_deref(),
                solana_config_path.as_deref(),
            )
            .unwrap_or(false)
            {
                // end condition reached successfully
                break;
            } else if (Instant::now() - loop_start).as_secs() > config.ci_mode_timeout {
                // we ran out of time
                log!("CI timeout reached before queues emptied");
                failure_occurred = true;
                break;
            }
        }

        // verify long-running tasks are still running
        for (name, (child, _)) in state.agents.iter_mut() {
            if let Some(status) = child.try_wait().unwrap() {
                if !status.success() {
                    log!(
                        "Child process {} exited unexpectedly, with code {}. Shutting down",
                        name,
                        status.code().unwrap()
                    );
                    failure_occurred = true;
                    SHUTDOWN.store(true, Ordering::Relaxed);
                    break;
                }
            }
        }

        sleep(Duration::from_secs(5));
    }

    // test retry request
    let resp = server::run_retry_request().expect("Failed to process retry request");
    assert!(resp.matched > 0);

    report_test_result(failure_occurred)
}

fn report_test_result(failure_occurred: bool) -> ExitCode {
    if failure_occurred {
        log!("E2E tests failed");
        ExitCode::FAILURE
    } else {
        log!("E2E tests passed");
        ExitCode::SUCCESS
    }
}
