use async_std::task::spawn_blocking;
use espresso_validator::{ConsensusConfig, NodeOpt};
use jf_cap::keys::UserPubKey;
use std::io::{BufRead, BufReader};
use std::path::PathBuf;
use std::process::{Command, ExitStatus, Stdio};
use structopt::StructOpt;

#[derive(StructOpt)]
#[structopt(
    name = "Multi-machine consensus",
    about = "Simulates consensus among multiple machines"
)]
struct Options {
    #[structopt(flatten)]
    node_opt: NodeOpt,

    /// Path to the node configuration file.
    #[structopt(long = "config", short = "c")]
    pub config: Option<PathBuf>,

    /// Path to the universal parameter file.
    #[structopt(long = "universal_param_path", short = "u")]
    pub universal_param_path: Option<String>,

    /// Public key which should own a faucet record in the genesis block.
    ///
    /// For each given public key, the ledger will be initialized with a record of 2^32 native
    /// tokens, owned by the public key.
    ///
    /// This option may be passed multiple times to initialize the ledger with multiple native
    /// token records.
    #[structopt(long)]
    pub faucet_pub_key: Vec<UserPubKey>,

    /// Number of transactions to generate.
    ///
    /// If not provided, the validator will wait for externally submitted transactions.
    #[structopt(long = "num_txn", short = "n", conflicts_with("faucet_pub_key"))]
    pub num_txn: Option<u64>,

    /// Wait for web server to exit after transactions complete.
    #[structopt(long)]
    pub wait: bool,

    #[structopt(short, long)]
    verbose: bool,

    /// Number of nodes to run only `fail_after_txn` rounds.
    ///
    /// If not provided, all nodes will keep running till `num_txn` rounds are completed.
    #[structopt(long = "num_fail_nodes")]
    num_fail_nodes: Option<u64>,

    /// Number of rounds that all nodes will be running, after which `num_fail_nodes` nodes will be
    /// killed.
    ///
    /// If not provided, all nodes will keep running till `num_txn` rounds are completed.
    #[structopt(long = "fail_after_txn")]
    fail_after_txn: Option<u64>,
}

#[async_std::main]
async fn main() {
    // Construct arguments to pass to the multi-machine demo.
    let options = Options::from_args();
    let mut args = vec![
        "--pk_path",
        &options.node_opt.pk_path,
        "--store_path",
        &options.node_opt.store_path,
        "--assets",
        &options.node_opt.web_path,
        "--api",
        &options.node_opt.api_path,
    ];
    if options.node_opt.load_from_store {
        args.push("--load_from_store");
    }
    if options.node_opt.full {
        args.push("--full");
    }
    if options.wait {
        args.push("--wait");
    }
    let config_path;
    if let Some(path) = &options.config {
        config_path = path.display().to_string();
        args.push("--config");
        args.push(&config_path);
    }
    let universal_param_path;
    if let Some(path) = &options.universal_param_path {
        universal_param_path = path.clone();
        args.push("--universal_param_path");
        args.push(&universal_param_path);
    }
    let faucet_pub_keys = options
        .faucet_pub_key
        .iter()
        .map(|k| k.to_string())
        .collect::<Vec<_>>();
    for pub_key in &faucet_pub_keys {
        args.push("--faucet_pub_key");
        args.push(pub_key);
    }
    let num_txn_str = match options.num_txn {
        Some(num_txn) => num_txn.to_string(),
        None => "".to_string(),
    };
    let (num_fail_nodes, fail_after_txn_str) = match options.num_fail_nodes {
        Some(num_fail_nodes) => {
            let fail_after_txn_str = options
                .fail_after_txn
                .expect("`fail_after_txn` isn't specified when `num_failed_nodes` is.")
                .to_string();
            (num_fail_nodes, fail_after_txn_str)
        }
        None => (0, "".to_string()),
    };

    // Read node info from node configuration file.
    let num_nodes = match &options.config {
        None => 7,
        Some(path) => ConsensusConfig::from_file(path).nodes.len(),
    };

    // Start the consensus for each node.
    let first_fail_id = num_nodes - num_fail_nodes as usize;
    let mut processes: Vec<_> = (0..num_nodes)
        .map(|id| {
            let mut this_args = args.clone();
            let id_str = id.to_string();
            this_args.push("--id");
            this_args.push(&id_str);
            if id >= first_fail_id as usize {
                this_args.push("--num_txn");
                this_args.push(&fail_after_txn_str);
            } else if !num_txn_str.is_empty() {
                this_args.push("--num_txn");
                this_args.push(&num_txn_str);
            }
            (
                id,
                Command::new("./espresso-validator")
                    .args(this_args)
                    .stdout(Stdio::piped())
                    .spawn()
                    .unwrap_or_else(|_| panic!("Failed to start the validator for node {}", id)),
            )
        })
        .collect();

    // Collect output from each process as they run. If we don't do this eagerly, validators can
    // block when their output pipes fill up causing deadlock.
    let outputs = processes
        .iter_mut()
        .map(|(id, p)| {
            let mut stdout = BufReader::new(p.stdout.take().unwrap());
            let id = *id;
            let verbose = options.verbose;
            spawn_blocking(move || {
                let mut lines = Vec::new();
                let mut line = String::new();
                loop {
                    if stdout
                        .read_line(&mut line)
                        .unwrap_or_else(|_| panic!("Failed to read stdout for node {}", id))
                        == 0
                    {
                        break;
                    }
                    if verbose {
                        print!("[{}] {}", id, line);
                    }
                    lines.push(std::mem::take(&mut line));
                }
                lines
            })
        })
        .collect::<Vec<_>>();

    // Check each process.
    let mut commitment = "".to_string();
    let mut succeeded_nodes = 0;
    for ((id, mut p), output) in processes.into_iter().zip(outputs) {
        println!("waiting for validator {}", id);
        let process: Result<ExitStatus, _> = p.wait();
        process.unwrap_or_else(|_| panic!("Failed to run the validator for node {}", id));
        // Check whether the commitments are the same.
        if let Some(num_txn) = options.num_txn {
            for line in output.await {
                let trimmed_line = line.trim();
                if trimmed_line.starts_with(&format!("- Round {} completed. Commitment:", num_txn))
                {
                    let strs: Vec<&str> = trimmed_line.split(' ').collect();
                    let comm = strs
                        .last()
                        .unwrap_or_else(|| panic!("Failed to parse commitment for node {}", id));
                    println!("Validator {} finished with commitment {}", id, comm);
                    if commitment.is_empty() {
                        commitment = comm.to_string();
                    } else {
                        assert_eq!(comm, &commitment);
                    }
                    succeeded_nodes += 1;
                    break;
                }
            }
        }
    }

    // Check whether the number of succeeded nodes meets the threshold.
    let threshold = ((num_nodes * 2) / 3) + 1;
    assert!(succeeded_nodes >= threshold);
    println!("Consensus completed for all nodes")
}
