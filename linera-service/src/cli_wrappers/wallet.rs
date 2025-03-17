// Copyright (c) Zefchain Labs, Inc.
// SPDX-License-Identifier: Apache-2.0

use std::{
    borrow::Cow,
    collections::BTreeMap,
    env,
    marker::PhantomData,
    mem,
    path::{Path, PathBuf},
    str::FromStr,
    sync,
    time::Duration,
};

use anyhow::{bail, ensure, Context, Result};
use async_graphql::InputType;
use async_tungstenite::tungstenite::{client::IntoClientRequest as _, http::HeaderValue};
use futures::{SinkExt as _, Stream, StreamExt as _, TryStreamExt as _};
use heck::ToKebabCase;
use linera_base::{
    abi::ContractAbi,
    command::{resolve_binary, CommandExt},
    crypto::CryptoHash,
    data_types::{Amount, Bytecode},
    identifiers::{Account, ApplicationId, ChainId, MessageId, ModuleId, Owner, UserApplicationId},
    vm::VmRuntime,
};
use linera_client::{client_options::ResourceControlPolicyConfig, wallet::Wallet};
use linera_core::worker::Notification;
use linera_execution::{
    committee::{Committee, Epoch},
    system::SystemChannel,
};
use linera_faucet::ClaimOutcome;
use linera_faucet_client::Faucet;
use serde::{de::DeserializeOwned, ser::Serialize};
use serde_json::{json, Value};
use tempfile::TempDir;
use tokio::process::{Child, Command};
use tracing::{error, info, warn};

use crate::{
    cli_wrappers::{
        local_net::{PathProvider, ProcessInbox},
        Network,
    },
    util::{self, ChildExt},
};

/// The name of the environment variable that allows specifying additional arguments to be passed
/// to the node-service command of the client.
const CLIENT_SERVICE_ENV: &str = "LINERA_CLIENT_SERVICE_PARAMS";

fn reqwest_client() -> reqwest::Client {
    reqwest::ClientBuilder::new()
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap()
}

/// Wrapper to run a Linera client command.
pub struct ClientWrapper {
    binary_path: sync::Mutex<Option<PathBuf>>,
    testing_prng_seed: Option<u64>,
    storage: String,
    wallet: String,
    max_pending_message_bundles: usize,
    network: Network,
    pub path_provider: PathProvider,
    on_drop: OnClientDrop,
}

/// Action to perform when the [`ClientWrapper`] is dropped.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum OnClientDrop {
    /// Close all the chains on the wallet.
    CloseChains,
    /// Do not close any chains, leaving them active.
    LeakChains,
}

impl ClientWrapper {
    pub fn new(
        path_provider: PathProvider,
        network: Network,
        testing_prng_seed: Option<u64>,
        id: usize,
        on_drop: OnClientDrop,
    ) -> Self {
        let storage = format!(
            "rocksdb:{}/client_{}.db",
            path_provider.path().display(),
            id
        );
        let wallet = format!("wallet_{}.json", id);
        Self {
            binary_path: sync::Mutex::new(None),
            testing_prng_seed,
            storage,
            wallet,
            max_pending_message_bundles: 10_000,
            network,
            path_provider,
            on_drop,
        }
    }

    /// Runs `linera project new`.
    pub async fn project_new(&self, project_name: &str, linera_root: &Path) -> Result<TempDir> {
        let tmp = TempDir::new()?;
        let mut command = self.command().await?;
        command
            .current_dir(tmp.path())
            .arg("project")
            .arg("new")
            .arg(project_name)
            .arg("--linera-root")
            .arg(linera_root)
            .spawn_and_wait_for_stdout()
            .await?;
        Ok(tmp)
    }

    /// Runs `linera project publish`.
    pub async fn project_publish<T: Serialize>(
        &self,
        path: PathBuf,
        required_application_ids: Vec<String>,
        publisher: impl Into<Option<ChainId>>,
        argument: &T,
    ) -> Result<String> {
        let json_parameters = serde_json::to_string(&())?;
        let json_argument = serde_json::to_string(argument)?;
        let mut command = self.command().await?;
        command
            .arg("project")
            .arg("publish-and-create")
            .arg(path)
            .args(publisher.into().iter().map(ChainId::to_string))
            .args(["--json-parameters", &json_parameters])
            .args(["--json-argument", &json_argument]);
        if !required_application_ids.is_empty() {
            command.arg("--required-application-ids");
            command.args(required_application_ids);
        }
        let stdout = command.spawn_and_wait_for_stdout().await?;
        Ok(stdout.trim().to_string())
    }

    /// Runs `linera project test`.
    pub async fn project_test(&self, path: &Path) -> Result<()> {
        self.command()
            .await
            .context("failed to create project test command")?
            .current_dir(path)
            .arg("project")
            .arg("test")
            .spawn_and_wait_for_stdout()
            .await?;
        Ok(())
    }

    async fn command(&self) -> Result<Command> {
        let mut command = self.command_binary().await?;
        command.current_dir(self.path_provider.path()).env(
            "RUST_LOG",
            std::env::var("RUST_LOG").unwrap_or(String::from("linera=debug")),
        );
        for argument in self.command_arguments() {
            command.arg(&*argument);
        }
        Ok(command)
    }

    /// Returns an iterator over the arguments that should be added to all command invocations.
    fn command_arguments(&self) -> impl Iterator<Item = Cow<'_, str>> + '_ {
        [
            "--wallet".into(),
            self.wallet.as_str().into(),
            "--storage".into(),
            self.storage.as_str().into(),
            "--max-pending-message-bundles".into(),
            self.max_pending_message_bundles.to_string().into(),
            "--send-timeout-ms".into(),
            "500000".into(),
            "--recv-timeout-ms".into(),
            "500000".into(),
            "--wait-for-outgoing-messages".into(),
        ]
        .into_iter()
    }

    /// Returns the [`Command`] instance configured to run the appropriate binary.
    ///
    /// The path is resolved once and cached inside `self` for subsequent usages.
    async fn command_binary(&self) -> Result<Command> {
        match self.command_with_cached_binary_path() {
            Some(command) => Ok(command),
            None => {
                let resolved_path = resolve_binary("linera", env!("CARGO_PKG_NAME")).await?;
                let command = Command::new(&resolved_path);

                self.set_cached_binary_path(resolved_path);

                Ok(command)
            }
        }
    }

    /// Returns a [`Command`] instance configured with the cached `binary_path`, if available.
    fn command_with_cached_binary_path(&self) -> Option<Command> {
        let binary_path = self.binary_path.lock().unwrap();

        binary_path.as_ref().map(Command::new)
    }

    /// Sets the cached `binary_path` with the `new_binary_path`.
    ///
    /// # Panics
    ///
    /// If the cache is already set to a different value. In theory the two threads calling
    /// `command_binary` can race and resolve the binary path twice, but they should always be the
    /// same path.
    fn set_cached_binary_path(&self, new_binary_path: PathBuf) {
        let mut binary_path = self.binary_path.lock().unwrap();

        if binary_path.is_none() {
            *binary_path = Some(new_binary_path);
        } else {
            assert_eq!(*binary_path, Some(new_binary_path));
        }
    }

    /// Runs `linera create-genesis-config`.
    pub async fn create_genesis_config(
        &self,
        num_other_initial_chains: u32,
        initial_funding: Amount,
        policy_config: ResourceControlPolicyConfig,
        http_allow_list: Option<Vec<String>>,
    ) -> Result<()> {
        let mut command = self.command().await?;
        command
            .args([
                "create-genesis-config",
                &num_other_initial_chains.to_string(),
            ])
            .args(["--initial-funding", &initial_funding.to_string()])
            .args(["--committee", "committee.json"])
            .args(["--genesis", "genesis.json"])
            .args([
                "--policy-config",
                &policy_config.to_string().to_kebab_case(),
            ]);
        if let Some(allow_list) = http_allow_list {
            command.arg("--http-allow-list").arg(allow_list.join(","));
        }
        if let Some(seed) = self.testing_prng_seed {
            command.arg("--testing-prng-seed").arg(seed.to_string());
        }
        command.spawn_and_wait_for_stdout().await?;
        Ok(())
    }

    /// Runs `linera wallet init`.
    pub async fn wallet_init(
        &self,
        chain_ids: &[ChainId],
        faucet: FaucetOption<'_>,
    ) -> Result<Option<(ClaimOutcome, Owner)>> {
        let mut command = self.command().await?;
        command.args(["wallet", "init"]);
        match faucet {
            FaucetOption::None => {
                command.args(["--genesis", "genesis.json"]);
            }
            FaucetOption::GenesisOnly(faucet) => {
                command.args(["--faucet", faucet.url()]);
            }
            FaucetOption::NewChain(faucet) => {
                command.args(["--with-new-chain", "--faucet", faucet.url()]);
            }
        }
        if let Some(seed) = self.testing_prng_seed {
            command.arg("--testing-prng-seed").arg(seed.to_string());
        }
        if !chain_ids.is_empty() {
            let ids = chain_ids.iter().map(ChainId::to_string);
            command.arg("--with-other-chains").args(ids);
        }
        let stdout = command.spawn_and_wait_for_stdout().await?;
        if matches!(faucet, FaucetOption::NewChain(_)) {
            let mut lines = stdout.split_whitespace();
            let chain_id_str = lines.next().context("missing chain ID")?;
            let message_id_str = lines.next().context("missing message ID")?;
            let certificate_hash_str = lines.next().context("missing certificate hash")?;
            let outcome = ClaimOutcome {
                chain_id: chain_id_str.parse().context("invalid chain ID")?,
                message_id: message_id_str.parse().context("invalid message ID")?,
                certificate_hash: certificate_hash_str
                    .parse()
                    .context("invalid certificate hash")?,
            };
            let owner = lines
                .next()
                .context("missing chain owner")?
                .parse()
                .context("invalid chain owner")?;
            Ok(Some((outcome, owner)))
        } else {
            Ok(None)
        }
    }

    /// Runs `linera wallet request-chain`.
    pub async fn request_chain(
        &self,
        faucet: &Faucet,
        set_default: bool,
    ) -> Result<(ClaimOutcome, Owner)> {
        let mut command = self.command().await?;
        command.args(["wallet", "request-chain", "--faucet", faucet.url()]);
        if set_default {
            command.arg("--set-default");
        }
        let stdout = command.spawn_and_wait_for_stdout().await?;
        let mut lines = stdout.split_whitespace();
        let chain_id_str = lines.next().context("missing chain ID")?;
        let message_id_str = lines.next().context("missing message ID")?;
        let certificate_hash_str = lines.next().context("missing certificate hash")?;
        let outcome = ClaimOutcome {
            chain_id: chain_id_str.parse().context("invalid chain ID")?,
            message_id: message_id_str.parse().context("invalid message ID")?,
            certificate_hash: certificate_hash_str
                .parse()
                .context("invalid certificate hash")?,
        };
        let owner = lines
            .next()
            .context("missing chain owner")?
            .parse()
            .context("invalid chain owner")?;
        Ok((outcome, owner))
    }

    /// Runs `linera wallet publish-and-create`.
    #[allow(clippy::too_many_arguments)]
    pub async fn publish_and_create<
        A: ContractAbi,
        Parameters: Serialize,
        InstantiationArgument: Serialize,
    >(
        &self,
        contract: PathBuf,
        service: PathBuf,
        vm_runtime: VmRuntime,
        parameters: &Parameters,
        argument: &InstantiationArgument,
        required_application_ids: &[UserApplicationId],
        publisher: impl Into<Option<ChainId>>,
    ) -> Result<ApplicationId<A>> {
        let json_parameters = serde_json::to_string(parameters)?;
        let json_argument = serde_json::to_string(argument)?;
        let mut command = self.command().await?;
        let vm_runtime = format!("{}", vm_runtime);
        command
            .arg("publish-and-create")
            .args([contract, service])
            .args(["--vm-runtime", &vm_runtime.to_lowercase()])
            .args(publisher.into().iter().map(ChainId::to_string))
            .args(["--json-parameters", &json_parameters])
            .args(["--json-argument", &json_argument]);
        if !required_application_ids.is_empty() {
            command.arg("--required-application-ids");
            command.args(
                required_application_ids
                    .iter()
                    .map(UserApplicationId::to_string),
            );
        }
        let stdout = command.spawn_and_wait_for_stdout().await?;
        Ok(stdout.trim().parse::<UserApplicationId>()?.with_abi())
    }

    /// Runs `linera publish-module`.
    pub async fn publish_module<Abi, Parameters, InstantiationArgument>(
        &self,
        contract: PathBuf,
        service: PathBuf,
        publisher: impl Into<Option<ChainId>>,
    ) -> Result<ModuleId<Abi, Parameters, InstantiationArgument>> {
        let stdout = self
            .command()
            .await?
            .arg("publish-module")
            .args([contract, service])
            .args(publisher.into().iter().map(ChainId::to_string))
            .spawn_and_wait_for_stdout()
            .await?;
        let module_id: ModuleId = stdout.trim().parse()?;
        Ok(module_id.with_abi())
    }

    /// Runs `linera create-application`.
    pub async fn create_application<
        Abi: ContractAbi,
        Parameters: Serialize,
        InstantiationArgument: Serialize,
    >(
        &self,
        module_id: &ModuleId<Abi, Parameters, InstantiationArgument>,
        parameters: &Parameters,
        argument: &InstantiationArgument,
        required_application_ids: &[UserApplicationId],
        creator: impl Into<Option<ChainId>>,
    ) -> Result<ApplicationId<Abi>> {
        let json_parameters = serde_json::to_string(parameters)?;
        let json_argument = serde_json::to_string(argument)?;
        let mut command = self.command().await?;
        command
            .arg("create-application")
            .arg(module_id.forget_abi().to_string())
            .args(["--json-parameters", &json_parameters])
            .args(["--json-argument", &json_argument])
            .args(creator.into().iter().map(ChainId::to_string));
        if !required_application_ids.is_empty() {
            command.arg("--required-application-ids");
            command.args(
                required_application_ids
                    .iter()
                    .map(UserApplicationId::to_string),
            );
        }
        let stdout = command.spawn_and_wait_for_stdout().await?;
        Ok(stdout.trim().parse::<UserApplicationId>()?.with_abi())
    }

    /// Runs `linera service`.
    pub async fn run_node_service(
        &self,
        port: impl Into<Option<u16>>,
        process_inbox: ProcessInbox,
    ) -> Result<NodeService> {
        let port = port.into().unwrap_or(8080);
        let mut command = self.command().await?;
        command.arg("service");
        if let ProcessInbox::Skip = process_inbox {
            command.arg("--listener-skip-process-inbox");
        }
        if let Ok(var) = env::var(CLIENT_SERVICE_ENV) {
            command.args(var.split_whitespace());
        }
        let child = command
            .args(["--port".to_string(), port.to_string()])
            .spawn_into()?;
        let client = reqwest_client();
        for i in 0..10 {
            linera_base::time::timer::sleep(Duration::from_secs(i)).await;
            let request = client
                .get(format!("http://localhost:{}/", port))
                .send()
                .await;
            if request.is_ok() {
                info!("Node service has started");
                return Ok(NodeService::new(port, child));
            } else {
                warn!("Waiting for node service to start");
            }
        }
        bail!("Failed to start node service");
    }

    /// Runs `linera query-validator`
    pub async fn query_validator(&self, address: &str) -> Result<CryptoHash> {
        let mut command = self.command().await?;
        command.arg("query-validator").arg(address);
        let stdout = command.spawn_and_wait_for_stdout().await?;
        let hash = stdout
            .trim()
            .parse()
            .context("error while parsing the result of `linera query-validator`")?;
        Ok(hash)
    }

    /// Runs `linera query-validators`.
    pub async fn query_validators(&self, chain_id: Option<ChainId>) -> Result<()> {
        let mut command = self.command().await?;
        command.arg("query-validators");
        if let Some(chain_id) = chain_id {
            command.arg(chain_id.to_string());
        }
        command.spawn_and_wait_for_stdout().await?;
        Ok(())
    }

    /// Runs `linera sync-validator`.
    pub async fn sync_validator(
        &self,
        chain_ids: impl IntoIterator<Item = &ChainId>,
        validator_address: impl Into<String>,
    ) -> Result<()> {
        let mut command = self.command().await?;
        command.arg("sync-validator").arg(validator_address.into());
        let mut chain_ids = chain_ids.into_iter().peekable();
        if chain_ids.peek().is_some() {
            command
                .arg("--chains")
                .args(chain_ids.map(ChainId::to_string));
        }
        command.spawn_and_wait_for_stdout().await?;
        Ok(())
    }

    /// Runs `linera faucet`.
    pub async fn run_faucet(
        &self,
        port: impl Into<Option<u16>>,
        chain_id: ChainId,
        amount: Amount,
    ) -> Result<FaucetService> {
        let port = port.into().unwrap_or(8080);
        let mut command = self.command().await?;
        let child = command
            .arg("faucet")
            .arg(chain_id.to_string())
            .args(["--port".to_string(), port.to_string()])
            .args(["--amount".to_string(), amount.to_string()])
            .spawn_into()?;
        let client = reqwest_client();
        for i in 0..10 {
            linera_base::time::timer::sleep(Duration::from_secs(i)).await;
            let request = client
                .get(format!("http://localhost:{}/", port))
                .send()
                .await;
            if request.is_ok() {
                info!("Faucet has started");
                return Ok(FaucetService::new(port, child));
            } else {
                warn!("Waiting for faucet to start");
            }
        }
        bail!("Failed to start faucet");
    }

    /// Runs `linera local-balance`.
    pub async fn local_balance(&self, account: Account) -> Result<Amount> {
        let stdout = self
            .command()
            .await?
            .arg("local-balance")
            .arg(account.to_string())
            .spawn_and_wait_for_stdout()
            .await?;
        let amount = stdout
            .trim()
            .parse()
            .context("error while parsing the result of `linera local-balance`")?;
        Ok(amount)
    }

    /// Runs `linera query-balance`.
    pub async fn query_balance(&self, account: Account) -> Result<Amount> {
        let stdout = self
            .command()
            .await?
            .arg("query-balance")
            .arg(account.to_string())
            .spawn_and_wait_for_stdout()
            .await?;
        let amount = stdout
            .trim()
            .parse()
            .context("error while parsing the result of `linera query-balance`")?;
        Ok(amount)
    }

    /// Runs `linera sync`.
    pub async fn sync(&self, chain_id: ChainId) -> Result<()> {
        self.command()
            .await?
            .arg("sync")
            .arg(chain_id.to_string())
            .spawn_and_wait_for_stdout()
            .await?;
        Ok(())
    }

    /// Runs `linera process-inbox`.
    pub async fn process_inbox(&self, chain_id: ChainId) -> Result<()> {
        self.command()
            .await?
            .arg("process-inbox")
            .arg(chain_id.to_string())
            .spawn_and_wait_for_stdout()
            .await?;
        Ok(())
    }

    /// Runs `linera transfer`.
    pub async fn transfer(&self, amount: Amount, from: ChainId, to: ChainId) -> Result<()> {
        self.command()
            .await?
            .arg("transfer")
            .arg(amount.to_string())
            .args(["--from", &from.to_string()])
            .args(["--to", &to.to_string()])
            .spawn_and_wait_for_stdout()
            .await?;
        Ok(())
    }

    /// Runs `linera transfer` with no logging.
    pub async fn transfer_with_silent_logs(
        &self,
        amount: Amount,
        from: ChainId,
        to: ChainId,
    ) -> Result<()> {
        self.command()
            .await?
            .env("RUST_LOG", "off")
            .arg("transfer")
            .arg(amount.to_string())
            .args(["--from", &from.to_string()])
            .args(["--to", &to.to_string()])
            .spawn_and_wait_for_stdout()
            .await?;
        Ok(())
    }

    /// Runs `linera transfer` with owner accounts.
    pub async fn transfer_with_accounts(
        &self,
        amount: Amount,
        from: Account,
        to: Account,
    ) -> Result<()> {
        self.command()
            .await?
            .arg("transfer")
            .arg(amount.to_string())
            .args(["--from", &from.to_string()])
            .args(["--to", &to.to_string()])
            .spawn_and_wait_for_stdout()
            .await?;
        Ok(())
    }

    /// Runs `linera benchmark`.
    #[cfg(feature = "benchmark")]
    pub async fn benchmark(
        &self,
        num_chains: usize,
        transactions_per_block: usize,
        fungible_application_id: Option<
            ApplicationId<linera_sdk::abis::fungible::FungibleTokenAbi>,
        >,
    ) -> Result<()> {
        let mut command = self.command().await?;
        command
            .arg("benchmark")
            .args(["--num-chains", &num_chains.to_string()])
            .args([
                "--transactions-per-block",
                &transactions_per_block.to_string(),
            ]);
        if let Some(application_id) = fungible_application_id {
            let application_id = application_id.forget_abi().to_string();
            command.args(["--fungible-application-id", &application_id]);
        }
        command.spawn_and_wait_for_stdout().await?;
        Ok(())
    }

    /// Runs `linera open-chain`.
    pub async fn open_chain(
        &self,
        from: ChainId,
        owner: Option<Owner>,
        initial_balance: Amount,
    ) -> Result<(MessageId, ChainId, Owner)> {
        let mut command = self.command().await?;
        command
            .arg("open-chain")
            .args(["--from", &from.to_string()])
            .args(["--initial-balance", &initial_balance.to_string()]);

        if let Some(owner) = owner {
            command.args(["--owner", &owner.to_string()]);
        }

        let stdout = command.spawn_and_wait_for_stdout().await?;
        let mut split = stdout.split('\n');
        let message_id: MessageId = split.next().context("no message ID in output")?.parse()?;
        let chain_id = ChainId::from_str(split.next().context("no chain ID in output")?)?;
        let new_owner = Owner::from_str(split.next().context("no owner in output")?)?;
        if let Some(owner) = owner {
            assert_eq!(owner, new_owner);
        }
        Ok((message_id, chain_id, new_owner))
    }

    /// Runs `linera open-chain` then `linera assign`.
    pub async fn open_and_assign(
        &self,
        client: &ClientWrapper,
        initial_balance: Amount,
    ) -> Result<ChainId> {
        let our_chain = self
            .load_wallet()?
            .default_chain()
            .context("no default chain found")?;
        let owner = client.keygen().await?;
        let (message_id, new_chain, _) = self
            .open_chain(our_chain, Some(owner), initial_balance)
            .await?;
        assert_eq!(new_chain, client.assign(owner, message_id).await?);
        Ok(new_chain)
    }

    pub async fn open_multi_owner_chain(
        &self,
        from: ChainId,
        owners: Vec<Owner>,
        weights: Vec<u64>,
        multi_leader_rounds: u32,
        balance: Amount,
        base_timeout_ms: u64,
    ) -> Result<(MessageId, ChainId)> {
        let mut command = self.command().await?;
        command
            .arg("open-multi-owner-chain")
            .args(["--from", &from.to_string()])
            .arg("--owners")
            .args(owners.iter().map(Owner::to_string))
            .args(["--base-timeout-ms", &base_timeout_ms.to_string()]);
        if !weights.is_empty() {
            command
                .arg("--owner-weights")
                .args(weights.iter().map(u64::to_string));
        };
        command
            .args(["--multi-leader-rounds", &multi_leader_rounds.to_string()])
            .args(["--initial-balance", &balance.to_string()]);

        let stdout = command.spawn_and_wait_for_stdout().await?;
        let mut split = stdout.split('\n');
        let message_id: MessageId = split.next().context("no message ID in output")?.parse()?;
        let chain_id = ChainId::from_str(split.next().context("no chain ID in output")?)?;

        Ok((message_id, chain_id))
    }

    pub async fn change_ownership(
        &self,
        chain_id: ChainId,
        super_owners: Vec<Owner>,
        owners: Vec<Owner>,
    ) -> Result<()> {
        let mut command = self.command().await?;
        command
            .arg("change-ownership")
            .args(["--chain-id", &chain_id.to_string()]);
        if !super_owners.is_empty() {
            command
                .arg("--super-owners")
                .args(super_owners.iter().map(Owner::to_string));
        }
        if !owners.is_empty() {
            command
                .arg("--owners")
                .args(owners.iter().map(Owner::to_string));
        }
        command.spawn_and_wait_for_stdout().await?;
        Ok(())
    }

    /// Runs `linera wallet follow-chain CHAIN_ID`.
    pub async fn follow_chain(&self, chain_id: ChainId) -> Result<()> {
        let mut command = self.command().await?;
        command
            .args(["wallet", "follow-chain"])
            .arg(chain_id.to_string());
        command.spawn_and_wait_for_stdout().await?;
        Ok(())
    }

    /// Runs `linera wallet forget-chain CHAIN_ID`.
    pub async fn forget_chain(&self, chain_id: ChainId) -> Result<()> {
        let mut command = self.command().await?;
        command
            .args(["wallet", "forget-chain"])
            .arg(chain_id.to_string());
        command.spawn_and_wait_for_stdout().await?;
        Ok(())
    }

    pub async fn retry_pending_block(
        &self,
        chain_id: Option<ChainId>,
    ) -> Result<Option<CryptoHash>> {
        let mut command = self.command().await?;
        command.arg("retry-pending-block");
        if let Some(chain_id) = chain_id {
            command.arg(chain_id.to_string());
        }
        let stdout = command.spawn_and_wait_for_stdout().await?;
        let stdout = stdout.trim();
        if stdout.is_empty() {
            Ok(None)
        } else {
            Ok(Some(CryptoHash::from_str(stdout)?))
        }
    }

    /// Runs `linera publish-data-blob`.
    pub async fn publish_data_blob(
        &self,
        path: &Path,
        chain_id: Option<ChainId>,
    ) -> Result<CryptoHash> {
        let mut command = self.command().await?;
        command.arg("publish-data-blob").arg(path);
        if let Some(chain_id) = chain_id {
            command.arg(chain_id.to_string());
        }
        let stdout = command.spawn_and_wait_for_stdout().await?;
        let stdout = stdout.trim();
        Ok(CryptoHash::from_str(stdout)?)
    }

    /// Runs `linera read-data-blob`.
    pub async fn read_data_blob(&self, hash: CryptoHash, chain_id: Option<ChainId>) -> Result<()> {
        let mut command = self.command().await?;
        command.arg("read-data-blob").arg(hash.to_string());
        if let Some(chain_id) = chain_id {
            command.arg(chain_id.to_string());
        }
        command.spawn_and_wait_for_stdout().await?;
        Ok(())
    }

    pub fn load_wallet(&self) -> Result<Wallet> {
        util::read_json(self.wallet_path())
    }

    pub fn wallet_path(&self) -> PathBuf {
        self.path_provider.path().join(&self.wallet)
    }

    pub fn storage_path(&self) -> &str {
        &self.storage
    }

    pub fn get_owner(&self) -> Option<Owner> {
        let wallet = self.load_wallet().ok()?;
        let chain_id = wallet.default_chain()?;
        let public_key = wallet.get(chain_id)?.key_pair.as_ref()?.public();
        Some(public_key.into())
    }

    pub async fn is_chain_present_in_wallet(&self, chain: ChainId) -> bool {
        self.load_wallet()
            .ok()
            .is_some_and(|wallet| wallet.get(chain).is_some())
    }

    pub async fn set_validator(
        &self,
        validator_key: &(String, String),
        port: usize,
        votes: usize,
    ) -> Result<()> {
        let address = format!("{}:127.0.0.1:{}", self.network.short(), port);
        self.command()
            .await?
            .arg("set-validator")
            .args(["--public-key", &validator_key.0])
            .args(["--account-key", &validator_key.1])
            .args(["--address", &address])
            .args(["--votes", &votes.to_string()])
            .spawn_and_wait_for_stdout()
            .await?;
        Ok(())
    }

    pub async fn remove_validator(&self, validator_key: &str) -> Result<()> {
        self.command()
            .await?
            .arg("remove-validator")
            .args(["--public-key", validator_key])
            .spawn_and_wait_for_stdout()
            .await?;
        Ok(())
    }

    pub async fn finalize_committee(&self) -> Result<()> {
        self.command()
            .await?
            .arg("finalize-committee")
            .spawn_and_wait_for_stdout()
            .await?;
        Ok(())
    }

    /// Runs `linera keygen`.
    pub async fn keygen(&self) -> Result<Owner> {
        let stdout = self
            .command()
            .await?
            .arg("keygen")
            .spawn_and_wait_for_stdout()
            .await?;
        Ok(Owner::from_str(stdout.trim())?)
    }

    /// Returns the default chain.
    pub fn default_chain(&self) -> Option<ChainId> {
        self.load_wallet().ok()?.default_chain()
    }

    /// Runs `linera assign`.
    pub async fn assign(&self, owner: Owner, message_id: MessageId) -> Result<ChainId> {
        let stdout = self
            .command()
            .await?
            .arg("assign")
            .args(["--owner", &owner.to_string()])
            .args(["--message-id", &message_id.to_string()])
            .spawn_and_wait_for_stdout()
            .await?;

        let chain_id = ChainId::from_str(stdout.trim())?;

        Ok(chain_id)
    }

    pub async fn build_application(
        &self,
        path: &Path,
        name: &str,
        is_workspace: bool,
    ) -> Result<(PathBuf, PathBuf)> {
        Command::new("cargo")
            .current_dir(self.path_provider.path())
            .arg("build")
            .arg("--release")
            .args(["--target", "wasm32-unknown-unknown"])
            .arg("--manifest-path")
            .arg(path.join("Cargo.toml"))
            .spawn_and_wait_for_stdout()
            .await?;

        let release_dir = match is_workspace {
            true => path.join("../target/wasm32-unknown-unknown/release"),
            false => path.join("target/wasm32-unknown-unknown/release"),
        };

        let contract = release_dir.join(format!("{}_contract.wasm", name.replace('-', "_")));
        let service = release_dir.join(format!("{}_service.wasm", name.replace('-', "_")));

        let contract_size = fs_err::tokio::metadata(&contract).await?.len();
        let service_size = fs_err::tokio::metadata(&service).await?.len();
        info!("Done building application {name}: contract_size={contract_size}, service_size={service_size}");

        Ok((contract, service))
    }
}

impl Drop for ClientWrapper {
    fn drop(&mut self) {
        use std::process::Command as SyncCommand;

        if self.on_drop != OnClientDrop::CloseChains {
            return;
        }

        let Ok(binary_path) = self.binary_path.lock() else {
            error!("Failed to close chains because a thread panicked with a lock to `binary_path`");
            return;
        };

        let Some(binary_path) = binary_path.as_ref() else {
            warn!(
                "Assuming no chains need to be closed, because the command binary was never \
                resolved and therefore presumably never called"
            );
            return;
        };

        let working_directory = self.path_provider.path();
        let mut wallet_show_command = SyncCommand::new(binary_path);

        for argument in self.command_arguments() {
            wallet_show_command.arg(&*argument);
        }

        let Ok(wallet_show_output) = wallet_show_command
            .current_dir(working_directory)
            .args(["wallet", "show", "--short", "--owned"])
            .output()
        else {
            warn!("Failed to execute `wallet show --short` to list chains to close");
            return;
        };

        if !wallet_show_output.status.success() {
            warn!("Failed to list chains in the wallet to close them");
            return;
        }

        let Ok(chain_list_string) = String::from_utf8(wallet_show_output.stdout) else {
            warn!(
                "Failed to close chains because `linera wallet show --short` \
                returned a non-UTF-8 output"
            );
            return;
        };

        let chain_ids = chain_list_string
            .split('\n')
            .map(|line| line.trim())
            .filter(|line| !line.is_empty());

        for chain_id in chain_ids {
            let mut close_chain_command = SyncCommand::new(binary_path);

            for argument in self.command_arguments() {
                close_chain_command.arg(&*argument);
            }

            close_chain_command.current_dir(working_directory);

            match close_chain_command.args(["close-chain", chain_id]).status() {
                Ok(status) if status.success() => (),
                Ok(failure) => warn!("Failed to close chain {chain_id}: {failure}"),
                Err(error) => warn!("Failed to close chain {chain_id}: {error}"),
            }
        }
    }
}

/// Whether `wallet_init` should use a faucet.
#[derive(Clone, Copy, Debug)]
pub enum FaucetOption<'a> {
    None,
    GenesisOnly(&'a Faucet),
    NewChain(&'a Faucet),
}

#[cfg(with_testing)]
impl ClientWrapper {
    pub async fn build_example(&self, name: &str) -> Result<(PathBuf, PathBuf)> {
        self.build_application(Self::example_path(name)?.as_path(), name, true)
            .await
    }

    pub fn example_path(name: &str) -> Result<PathBuf> {
        Ok(env::current_dir()?.join("../examples/").join(name))
    }
}

fn truncate_query_output(input: &str) -> String {
    let max_len = 200;
    if input.len() < max_len {
        input.to_string()
    } else {
        format!("{} ...", input.get(..max_len).unwrap())
    }
}

/// A running node service.
pub struct NodeService {
    port: u16,
    child: Child,
}

impl NodeService {
    fn new(port: u16, child: Child) -> Self {
        Self { port, child }
    }

    pub async fn terminate(mut self) -> Result<()> {
        self.child.kill().await.context("terminating node service")
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn ensure_is_running(&mut self) -> Result<()> {
        self.child.ensure_is_running()
    }

    pub async fn process_inbox(&self, chain_id: &ChainId) -> Result<Vec<CryptoHash>> {
        let query = format!("mutation {{ processInbox(chainId: \"{chain_id}\") }}");
        let mut data = self.query_node(query).await?;
        Ok(serde_json::from_value(data["processInbox"].take())?)
    }

    pub async fn make_application<A: ContractAbi>(
        &self,
        chain_id: &ChainId,
        application_id: &ApplicationId<A>,
    ) -> Result<ApplicationWrapper<A>> {
        let application_id = application_id.forget_abi().to_string();
        let link = format!(
            "http://localhost:{}/chains/{chain_id}/applications/{application_id}",
            self.port
        );
        Ok(ApplicationWrapper::from(link))
    }

    pub async fn publish_data_blob(
        &self,
        chain_id: &ChainId,
        bytes: Vec<u8>,
    ) -> Result<CryptoHash> {
        let query = format!(
            "mutation {{ publishDataBlob(chainId: {}, bytes: {}) }}",
            chain_id.to_value(),
            bytes.to_value(),
        );
        let data = self.query_node(query).await?;
        serde_json::from_value(data["publishDataBlob"].clone())
            .context("missing publishDataBlob field in response")
    }

    pub async fn publish_module<Abi, Parameters, InstantiationArgument>(
        &self,
        chain_id: &ChainId,
        contract: PathBuf,
        service: PathBuf,
        vm_runtime: VmRuntime,
    ) -> Result<ModuleId<Abi, Parameters, InstantiationArgument>> {
        let contract_code = Bytecode::load_from_file(&contract).await?;
        let service_code = Bytecode::load_from_file(&service).await?;
        let query = format!(
            "mutation {{ publishModule(chainId: {}, contract: {}, service: {}, vmRuntime: {}) }}",
            chain_id.to_value(),
            contract_code.to_value(),
            service_code.to_value(),
            vm_runtime.to_value(),
        );
        let data = self.query_node(query).await?;
        let module_str = data["publishModule"]
            .as_str()
            .context("module ID not found")?;
        let module_id: ModuleId = module_str.parse().context("could not parse module ID")?;
        Ok(module_id.with_abi())
    }

    pub async fn query_committees(&self, chain_id: &ChainId) -> Result<BTreeMap<Epoch, Committee>> {
        let query = format!(
            "query {{ chain(chainId:\"{chain_id}\") {{
                executionState {{ system {{ committees }} }}
            }} }}"
        );
        let mut response = self.query_node(query).await?;
        let committees = response["chain"]["executionState"]["system"]["committees"].take();
        Ok(serde_json::from_value(committees)?)
    }

    pub async fn query_node(&self, query: impl AsRef<str>) -> Result<Value> {
        let n_try = 5;
        let query = query.as_ref();
        for i in 0..n_try {
            linera_base::time::timer::sleep(Duration::from_secs(i)).await;
            let url = format!("http://localhost:{}/", self.port);
            let client = reqwest_client();
            let result = client
                .post(url)
                .json(&json!({ "query": query }))
                .send()
                .await;
            if matches!(result, Err(ref error) if error.is_timeout()) {
                warn!("Timeout when sending query {query:?} to the node service");
                continue;
            }
            let response = result.with_context(|| {
                format!(
                    "query_node: failed to post query={}",
                    truncate_query_output(query)
                )
            })?;
            anyhow::ensure!(
                response.status().is_success(),
                "Query \"{}\" failed: {}",
                truncate_query_output(query),
                response
                    .text()
                    .await
                    .unwrap_or_else(|error| format!("Could not get response text: {error}"))
            );
            let value: Value = response.json().await.context("invalid JSON")?;
            if let Some(errors) = value.get("errors") {
                warn!(
                    "Query \"{}\" failed: {}",
                    truncate_query_output(query),
                    errors
                );
            } else {
                return Ok(value["data"].clone());
            }
        }
        bail!(
            "Query \"{}\" failed after {} retries.",
            truncate_query_output(query),
            n_try
        );
    }

    pub async fn create_application<
        Abi: ContractAbi,
        Parameters: Serialize,
        InstantiationArgument: Serialize,
    >(
        &self,
        chain_id: &ChainId,
        module_id: &ModuleId<Abi, Parameters, InstantiationArgument>,
        parameters: &Parameters,
        argument: &InstantiationArgument,
        required_application_ids: &[UserApplicationId],
    ) -> Result<ApplicationId<Abi>> {
        let module_id = module_id.forget_abi();
        let json_required_applications_ids = required_application_ids
            .iter()
            .map(UserApplicationId::to_string)
            .collect::<Vec<_>>()
            .to_value();
        // Convert to `serde_json::Value` then `async_graphql::Value` via the trait `InputType`.
        let new_parameters = serde_json::to_value(parameters)
            .context("could not create parameters JSON")?
            .to_value();
        let new_argument = serde_json::to_value(argument)
            .context("could not create argument JSON")?
            .to_value();
        let query = format!(
            "mutation {{ createApplication(\
                 chainId: \"{chain_id}\",
                 moduleId: \"{module_id}\", \
                 parameters: {new_parameters}, \
                 instantiationArgument: {new_argument}, \
                 requiredApplicationIds: {json_required_applications_ids}) \
             }}"
        );
        let data = self.query_node(query).await?;
        let app_id_str = data["createApplication"]
            .as_str()
            .context("missing createApplication string in response")?
            .trim();
        Ok(app_id_str
            .parse::<UserApplicationId>()
            .context("invalid application ID")?
            .with_abi())
    }

    pub async fn subscribe(
        &self,
        subscriber_chain_id: ChainId,
        publisher_chain_id: ChainId,
        channel: SystemChannel,
    ) -> Result<()> {
        let query = format!(
            "mutation {{ subscribe(\
                 subscriberChainId: \"{subscriber_chain_id}\", \
                 publisherChainId: \"{publisher_chain_id}\", \
                 channel: \"{}\") \
             }}",
            channel.to_value(),
        );
        self.query_node(query).await?;
        Ok(())
    }

    /// Obtains the hash of the `chain`'s tip block, as known by this node service.
    pub async fn chain_tip_hash(&self, chain: ChainId) -> Result<Option<CryptoHash>> {
        let query = format!(r#"query {{ block(chainId: "{chain}") {{ hash }} }}"#);

        let mut response = self.query_node(&query).await?;

        match mem::take(&mut response["block"]["hash"]) {
            Value::Null => Ok(None),
            Value::String(hash) => Ok(Some(
                hash.parse()
                    .context("Received an invalid hash {hash:?} for chain tip")?,
            )),
            invalid_data => bail!("Expected a tip hash string, but got {invalid_data:?} instead"),
        }
    }

    /// Subscribes to the node service and returns a stream of notifications about a chain.
    pub async fn notifications(
        &self,
        chain_id: ChainId,
    ) -> Result<impl Stream<Item = Result<Notification>>> {
        let query = format!("subscription {{ notifications(chainId: \"{chain_id}\") }}",);
        let url = format!("ws://localhost:{}/ws", self.port);
        let mut request = url.into_client_request()?;
        request.headers_mut().insert(
            "Sec-WebSocket-Protocol",
            HeaderValue::from_str("graphql-transport-ws")?,
        );
        let (mut websocket, _) = async_tungstenite::tokio::connect_async(request).await?;
        let init_json = json!({
          "type": "connection_init",
          "payload": {}
        });
        websocket.send(init_json.to_string().into()).await?;
        let text = websocket
            .next()
            .await
            .context("Failed to establish connection")??
            .into_text()?;
        ensure!(
            text == "{\"type\":\"connection_ack\"}",
            "Unexpected response: {text}"
        );
        let query_json = json!({
          "id": "1",
          "type": "start",
          "payload": {
            "query": query,
            "variables": {},
            "operationName": null
          }
        });
        websocket.send(query_json.to_string().into()).await?;
        Ok(websocket
            .map_err(anyhow::Error::from)
            .and_then(|message| async {
                let text = message.into_text()?;
                let value: Value = serde_json::from_str(&text).context("invalid JSON")?;
                if let Some(errors) = value["payload"].get("errors") {
                    bail!("Notification subscription failed: {errors:?}");
                }
                serde_json::from_value(value["payload"]["data"]["notifications"].clone())
                    .context("Failed to deserialize notification")
            }))
    }
}

/// A running faucet service.
pub struct FaucetService {
    port: u16,
    child: Child,
}

impl FaucetService {
    fn new(port: u16, child: Child) -> Self {
        Self { port, child }
    }

    pub async fn terminate(mut self) -> Result<()> {
        self.child
            .kill()
            .await
            .context("terminating faucet service")
    }

    pub fn ensure_is_running(&mut self) -> Result<()> {
        self.child.ensure_is_running()
    }

    pub fn instance(&self) -> Faucet {
        Faucet::new(format!("http://localhost:{}/", self.port))
    }
}

/// A running `Application` to be queried in GraphQL.
pub struct ApplicationWrapper<A> {
    uri: String,
    _phantom: PhantomData<A>,
}

impl<A> ApplicationWrapper<A> {
    pub async fn raw_query(&self, query: impl AsRef<str>) -> Result<Value> {
        const MAX_RETRIES: usize = 5;

        for i in 0.. {
            let query = query.as_ref();
            let client = reqwest_client();
            let result = client
                .post(&self.uri)
                .json(&json!({ "query": query }))
                .send()
                .await;
            let response = match result {
                Ok(response) => response,
                Err(error) if i < MAX_RETRIES => {
                    warn!(
                        "Failed to post query \"{}\": {error}; retrying",
                        truncate_query_output(query),
                    );
                    continue;
                }
                Err(error) => {
                    return Err(error).with_context(|| {
                        format!(
                            "raw_query: failed to post query={}",
                            truncate_query_output(query)
                        )
                    });
                }
            };
            anyhow::ensure!(
                response.status().is_success(),
                "Query \"{}\" failed: {}",
                truncate_query_output(query),
                response
                    .text()
                    .await
                    .unwrap_or_else(|error| format!("Could not get response text: {error}"))
            );
            let value: Value = response.json().await.context("invalid JSON")?;
            if let Some(errors) = value.get("errors") {
                bail!(
                    "Query \"{}\" failed: {}",
                    truncate_query_output(query),
                    errors
                );
            }
            return Ok(value["data"].clone());
        }
        unreachable!()
    }

    pub async fn query(&self, query: impl AsRef<str>) -> Result<Value> {
        let query = query.as_ref();
        self.raw_query(&format!("query {{ {query} }}")).await
    }

    pub async fn query_json<T: DeserializeOwned>(&self, query: impl AsRef<str>) -> Result<T> {
        let query = query.as_ref().trim();
        let name = query
            .split_once(|ch: char| !ch.is_alphanumeric())
            .map_or(query, |(name, _)| name);
        let data = self.query(query).await?;
        serde_json::from_value(data[name].clone())
            .with_context(|| format!("{name} field missing in response"))
    }

    pub async fn mutate(&self, mutation: impl AsRef<str>) -> Result<Value> {
        let mutation = mutation.as_ref();
        self.raw_query(&format!("mutation {{ {mutation} }}")).await
    }
}

impl<A> From<String> for ApplicationWrapper<A> {
    fn from(uri: String) -> ApplicationWrapper<A> {
        ApplicationWrapper {
            uri,
            _phantom: PhantomData,
        }
    }
}
