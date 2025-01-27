//! Service and ServiceFactory implementation. Specialized wrapper over substrate service.
use futures::{future, StreamExt};
use std::{
    collections::BTreeMap,
    sync::{Arc, Mutex},
    time::Duration,
};

use sc_cli::SubstrateCli;
use sc_client_api::{BlockBackend, BlockchainEvents, ExecutorProvider};
use sc_consensus_aura::{ImportQueueParams, SlotProportion, StartAuraParams};
use sc_executor::NativeElseWasmExecutor;
use sc_finality_grandpa::SharedVoterState;
use sc_keystore::LocalKeystore;
use sc_service::{error::Error as ServiceError, BasePath, Configuration, TaskManager};
use sc_telemetry::{Telemetry, TelemetryWorker};
use sp_consensus_aura::sr25519::AuthorityPair as AuraPair;

// Frontier
use fc_consensus::FrontierBlockImport;
use fc_db::DatabaseSource;
use fc_mapping_sync::{MappingSyncWorker, SyncStrategy};
use fc_rpc::{EthTask, OverrideHandle};
use fc_rpc_core::types::{FeeHistoryCache, FeeHistoryCacheLimit, FilterPool};

use amax_eva_runtime::{self, NodeBlock as Block, RuntimeApi};
// Our native executor instance.
pub struct ExecutorDispatch;

impl sc_executor::NativeExecutionDispatch for ExecutorDispatch {
    /// Only enable the benchmarking host functions when we actually want to benchmark.
    #[cfg(feature = "runtime-benchmarks")]
    type ExtendHostFunctions = frame_benchmarking::benchmarking::HostFunctions;
    /// Otherwise we only use the default Substrate host functions.
    #[cfg(not(feature = "runtime-benchmarks"))]
    type ExtendHostFunctions = ();

    fn dispatch(method: &str, data: &[u8]) -> Option<Vec<u8>> {
        amax_eva_runtime::api::dispatch(method, data)
    }

    fn native_version() -> sc_executor::NativeVersion {
        amax_eva_runtime::native_version()
    }
}

pub(crate) type FullClient =
    sc_service::TFullClient<Block, RuntimeApi, NativeElseWasmExecutor<ExecutorDispatch>>;
type FullBackend = sc_service::TFullBackend<Block>;
type FullSelectChain = sc_consensus::LongestChain<FullBackend, Block>;

type BlockImport = FrontierBlockImport<
    Block,
    sc_finality_grandpa::GrandpaBlockImport<FullBackend, Block, FullClient, FullSelectChain>,
    FullClient,
>;

pub fn new_partial(
    config: &Configuration,
) -> Result<
    sc_service::PartialComponents<
        FullClient,
        FullBackend,
        FullSelectChain,
        sc_consensus::DefaultImportQueue<Block, FullClient>,
        sc_transaction_pool::FullPool<Block, FullClient>,
        (
            BlockImport,
            sc_finality_grandpa::LinkHalf<Block, FullClient, FullSelectChain>,
            Option<Telemetry>,
            Arc<fc_db::Backend<Block>>,
            Option<FilterPool>,
            (FeeHistoryCache, FeeHistoryCacheLimit),
        ),
    >,
    ServiceError,
> {
    if config.keystore_remote.is_some() {
        return Err(ServiceError::Other(
            "Remote Keystores are not supported.".into(),
        ));
    }

    let telemetry = config
        .telemetry_endpoints
        .clone()
        .filter(|x| !x.is_empty())
        .map(|endpoints| -> Result<_, sc_telemetry::Error> {
            let worker = TelemetryWorker::new(16)?;
            let telemetry = worker.handle().new_telemetry(endpoints);
            Ok((worker, telemetry))
        })
        .transpose()?;

    let executor = NativeElseWasmExecutor::<ExecutorDispatch>::new(
        config.wasm_method,
        config.default_heap_pages,
        config.max_runtime_instances,
        config.runtime_cache_size,
    );

    let (client, backend, keystore_container, task_manager) =
        sc_service::new_full_parts::<Block, RuntimeApi, _>(
            config,
            telemetry.as_ref().map(|(_, telemetry)| telemetry.handle()),
            executor,
        )?;
    let client = Arc::new(client);

    let telemetry = telemetry.map(|(worker, telemetry)| {
        task_manager
            .spawn_handle()
            .spawn("telemetry", None, worker.run());
        telemetry
    });

    let select_chain = sc_consensus::LongestChain::new(backend.clone());

    let transaction_pool = sc_transaction_pool::BasicPool::new_full(
        config.transaction_pool.clone(),
        config.role.is_authority().into(),
        config.prometheus_registry(),
        task_manager.spawn_essential_handle(),
        client.clone(),
    );

    let frontier_backend = open_frontier_backend(config)?;
    let filter_pool: Option<FilterPool> = Some(Arc::new(Mutex::new(BTreeMap::new())));
    let fee_history_cache: FeeHistoryCache = Arc::new(Mutex::new(BTreeMap::new()));
    // TODO get from cli
    let fee_history_cache_limit: FeeHistoryCacheLimit = 100000; // cli.run.fee_history_limit;

    let (grandpa_block_import, grandpa_link) = sc_finality_grandpa::block_import(
        client.clone(),
        &(client.clone() as Arc<_>),
        select_chain.clone(),
        telemetry.as_ref().map(|x| x.handle()),
    )?;

    // frontier part
    let frontier_block_import = FrontierBlockImport::new(
        grandpa_block_import.clone(),
        client.clone(),
        frontier_backend.clone(),
    );

    let slot_duration = sc_consensus_aura::slot_duration(&*client)?;

    let import_queue =
        sc_consensus_aura::import_queue::<AuraPair, _, _, _, _, _, _>(ImportQueueParams {
            block_import: frontier_block_import.clone(),
            justification_import: Some(Box::new(grandpa_block_import)),
            client: client.clone(),
            create_inherent_data_providers: move |_, ()| async move {
                let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

                let slot =
					sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_slot_duration(
						*timestamp,
						slot_duration,
					);

                Ok((timestamp, slot))
            },
            spawner: &task_manager.spawn_essential_handle(),
            can_author_with: sp_consensus::CanAuthorWithNativeVersion::new(
                client.executor().clone(),
            ),
            registry: config.prometheus_registry(),
            check_for_equivocation: Default::default(),
            telemetry: telemetry.as_ref().map(|x| x.handle()),
        })?;

    Ok(sc_service::PartialComponents {
        client,
        backend,
        task_manager,
        import_queue,
        keystore_container,
        select_chain,
        transaction_pool,
        other: (
            frontier_block_import,
            grandpa_link,
            telemetry,
            frontier_backend,
            filter_pool,
            (fee_history_cache, fee_history_cache_limit),
        ),
    })
}

fn remote_keystore(_url: &str) -> Result<Arc<LocalKeystore>, &'static str> {
    // FIXME: here would the concrete keystore be built,
    //        must return a concrete type (NOT `LocalKeystore`) that
    //        implements `CryptoStore` and `SyncCryptoStore`
    Err("Remote Keystore not supported.")
}

/// Builds a new service for a full client.
pub fn new_full(mut config: Configuration) -> Result<TaskManager, ServiceError> {
    let sc_service::PartialComponents {
        client,
        backend,
        mut task_manager,
        import_queue,
        mut keystore_container,
        select_chain,
        transaction_pool,
        other:
            (
                block_import,
                grandpa_link,
                mut telemetry,
                frontier_backend,
                filter_pool,
                (fee_history_cache, fee_history_cache_limit),
            ),
    } = new_partial(&config)?;

    if let Some(url) = &config.keystore_remote {
        match remote_keystore(url) {
            Ok(k) => keystore_container.set_remote_keystore(k),
            Err(e) => {
                return Err(ServiceError::Other(format!(
                    "Error hooking up remote keystore for {}: {}",
                    url, e
                )))
            }
        };
    }
    let grandpa_protocol_name = sc_finality_grandpa::protocol_standard_name(
        &client
            .block_hash(0)
            .ok()
            .flatten()
            .expect("Genesis block exists; qed"),
        &config.chain_spec,
    );

    config
        .network
        .extra_sets
        .push(sc_finality_grandpa::grandpa_peers_set_config(
            grandpa_protocol_name.clone(),
        ));
    let warp_sync = Arc::new(sc_finality_grandpa::warp_proof::NetworkProvider::new(
        backend.clone(),
        grandpa_link.shared_authority_set().clone(),
        Vec::default(),
    ));

    let (network, system_rpc_tx, network_starter) =
        sc_service::build_network(sc_service::BuildNetworkParams {
            config: &config,
            client: client.clone(),
            transaction_pool: transaction_pool.clone(),
            spawn_handle: task_manager.spawn_handle(),
            import_queue,
            block_announce_validator_builder: None,
            warp_sync: Some(warp_sync),
        })?;

    if config.offchain_worker.enabled {
        sc_service::build_offchain_workers(
            &config,
            task_manager.spawn_handle(),
            client.clone(),
            network.clone(),
        );
    }

    let role = config.role.clone();
    let force_authoring = config.force_authoring;
    let backoff_authoring_blocks: Option<()> = None;
    let name = config.network.node_name.clone();
    let enable_grandpa = !config.disable_grandpa;
    let prometheus_registry = config.prometheus_registry().cloned();

    // frontier part.
    let overrides = crate::rpc::overrides_handle(client.clone());
    let block_data_cache = Arc::new(fc_rpc::EthBlockDataCacheTask::new(
        task_manager.spawn_handle(),
        overrides.clone(),
        50,
        50,
        prometheus_registry.clone(),
    ));

    let rpc_extensions_builder = {
        let client = client.clone();
        let pool = transaction_pool.clone();
        let is_authority = role.is_authority();
        // TODO get from cli
        let enable_dev_signer = true; // cli.run.enable_dev_signer;
        let network = network.clone();
        let filter_pool = filter_pool.clone();
        let frontier_backend = frontier_backend.clone();
        let overrides = overrides.clone();
        let fee_history_cache = fee_history_cache.clone();
        // TODO get from cli
        let max_past_logs = 1000; // cli.run.max_past_logs;
        let subscription_task_executor =
            sc_rpc::SubscriptionTaskExecutor::new(task_manager.spawn_handle());

        Box::new(move |deny_unsafe, _| {
            let deps = crate::rpc::FullDeps {
                client: client.clone(),
                pool: pool.clone(),
                deny_unsafe,
                graph: pool.pool().clone(),
                is_authority,
                enable_dev_signer,
                network: network.clone(),
                filter_pool: filter_pool.clone(),
                backend: frontier_backend.clone(),
                max_past_logs,
                fee_history_cache: fee_history_cache.clone(),
                fee_history_cache_limit,
                overrides: overrides.clone(),
                block_data_cache: block_data_cache.clone(),
            };

            Ok(crate::rpc::create_full(
                deps,
                subscription_task_executor.clone(),
            ))
        })
    };

    let _rpc_handlers = sc_service::spawn_tasks(sc_service::SpawnTasksParams {
        network: network.clone(),
        client: client.clone(),
        keystore: keystore_container.sync_keystore(),
        task_manager: &mut task_manager,
        transaction_pool: transaction_pool.clone(),
        rpc_extensions_builder,
        backend: backend.clone(),
        system_rpc_tx,
        config,
        telemetry: telemetry.as_mut(),
    })?;

    spawn_frontier_tasks(
        &task_manager,
        client.clone(),
        backend,
        frontier_backend,
        filter_pool,
        overrides,
        fee_history_cache,
        fee_history_cache_limit,
    );

    if role.is_authority() {
        let proposer_factory = sc_basic_authorship::ProposerFactory::new(
            task_manager.spawn_handle(),
            client.clone(),
            transaction_pool,
            prometheus_registry.as_ref(),
            telemetry.as_ref().map(|x| x.handle()),
        );

        let can_author_with =
            sp_consensus::CanAuthorWithNativeVersion::new(client.executor().clone());

        let slot_duration = sc_consensus_aura::slot_duration(&*client)?;

        let aura = sc_consensus_aura::start_aura::<AuraPair, _, _, _, _, _, _, _, _, _, _, _>(
            StartAuraParams {
                slot_duration,
                client,
                select_chain,
                block_import,
                proposer_factory,
                create_inherent_data_providers: move |_, ()| async move {
                    let timestamp = sp_timestamp::InherentDataProvider::from_system_time();

                    let slot =
						sp_consensus_aura::inherents::InherentDataProvider::from_timestamp_and_slot_duration(
							*timestamp,
							slot_duration,
						);

                    Ok((timestamp, slot))
                },
                force_authoring,
                backoff_authoring_blocks,
                keystore: keystore_container.sync_keystore(),
                can_author_with,
                sync_oracle: network.clone(),
                justification_sync_link: network.clone(),
                block_proposal_slot_portion: SlotProportion::new(2f32 / 3f32),
                max_block_proposal_slot_portion: None,
                telemetry: telemetry.as_ref().map(|x| x.handle()),
            },
        )?;

        // the AURA authoring task is considered essential, i.e. if it
        // fails we take down the service with it.
        task_manager
            .spawn_essential_handle()
            .spawn_blocking("aura", Some("block-authoring"), aura);
    }

    // if the node isn't actively participating in consensus then it doesn't
    // need a keystore, regardless of which protocol we use below.
    let keystore = if role.is_authority() {
        Some(keystore_container.sync_keystore())
    } else {
        None
    };

    let grandpa_config = sc_finality_grandpa::Config {
        // FIXME #1578 make this available through chainspec
        gossip_duration: Duration::from_millis(333),
        justification_period: 512,
        name: Some(name),
        observer_enabled: false,
        keystore,
        local_role: role,
        telemetry: telemetry.as_ref().map(|x| x.handle()),
        protocol_name: grandpa_protocol_name,
    };

    if enable_grandpa {
        // start the full GRANDPA voter
        // NOTE: non-authorities could run the GRANDPA observer protocol, but at
        // this point the full voter should provide better guarantees of block
        // and vote data availability than the observer. The observer has not
        // been tested extensively yet and having most nodes in a network run it
        // could lead to finality stalls.
        let grandpa_config = sc_finality_grandpa::GrandpaParams {
            config: grandpa_config,
            link: grandpa_link,
            network,
            voting_rule: sc_finality_grandpa::VotingRulesBuilder::default().build(),
            prometheus_registry,
            shared_voter_state: SharedVoterState::empty(),
            telemetry: telemetry.as_ref().map(|x| x.handle()),
        };

        // the GRANDPA voter task is considered infallible, i.e.
        // if it fails we take down the service with it.
        task_manager.spawn_essential_handle().spawn_blocking(
            "grandpa-voter",
            None,
            sc_finality_grandpa::run_grandpa_voter(grandpa_config)?,
        );
    }

    network_starter.start_network();
    Ok(task_manager)
}

pub fn frontier_database_dir(config: &Configuration, path: &str) -> std::path::PathBuf {
    let config_dir = config
        .base_path
        .as_ref()
        .map(|base_path| base_path.config_dir(config.chain_spec.id()))
        .unwrap_or_else(|| {
            BasePath::from_project("", "", &crate::cli::Cli::executable_name())
                .config_dir(config.chain_spec.id())
        });
    config_dir.join("frontier").join(path)
}

pub fn open_frontier_backend(config: &Configuration) -> Result<Arc<fc_db::Backend<Block>>, String> {
    Ok(Arc::new(fc_db::Backend::<Block>::new(
        &fc_db::DatabaseSettings {
            source: match config.database {
                DatabaseSource::RocksDb { .. } => DatabaseSource::RocksDb {
                    path: frontier_database_dir(config, "db"),
                    cache_size: 0,
                },
                DatabaseSource::ParityDb { .. } => DatabaseSource::ParityDb {
                    path: frontier_database_dir(config, "paritydb"),
                },
                DatabaseSource::Auto { .. } => DatabaseSource::Auto {
                    rocksdb_path: frontier_database_dir(config, "db"),
                    paritydb_path: frontier_database_dir(config, "paritydb"),
                    cache_size: 0,
                },
                _ => {
                    return Err("Supported db sources: `rocksdb` | `paritydb` | `auto`".to_string())
                }
            },
        },
    )?))
}

// Spawn frontier tasks.
pub fn spawn_frontier_tasks(
    task_manager: &TaskManager,
    client: Arc<FullClient>,
    backend: Arc<FullBackend>,
    frontier_backend: Arc<fc_db::Backend<Block>>,
    filter_pool: Option<FilterPool>,
    overrides: Arc<OverrideHandle<Block>>,
    fee_history_cache: FeeHistoryCache,
    fee_history_cache_limit: u64,
) {
    task_manager.spawn_essential_handle().spawn(
        "frontier-mapping-sync-worker",
        Some("frontier"),
        MappingSyncWorker::new(
            client.import_notification_stream(),
            Duration::new(2, 0),
            client.clone(),
            backend,
            frontier_backend.clone(),
            3,
            0,
            SyncStrategy::Normal,
        )
        .for_each(|()| future::ready(())),
    );

    // Spawn Frontier EthFilterApi maintenance task.
    if let Some(filter_pool) = filter_pool {
        // Each filter is allowed to stay in the pool for 100 blocks.
        const FILTER_RETAIN_THRESHOLD: u64 = 100;
        task_manager.spawn_essential_handle().spawn(
            "frontier-filter-pool",
            Some("frontier"),
            EthTask::filter_pool_task(client.clone(), filter_pool, FILTER_RETAIN_THRESHOLD),
        );
    }

    // Spawn Frontier FeeHistory cache maintenance task.
    task_manager.spawn_essential_handle().spawn(
        "frontier-fee-history",
        Some("frontier"),
        EthTask::fee_history_task(
            client.clone(),
            overrides,
            fee_history_cache,
            fee_history_cache_limit,
        ),
    );

    task_manager.spawn_essential_handle().spawn(
        "frontier-schema-cache-task",
        Some("frontier"),
        EthTask::ethereum_schema_cache_task(client, frontier_backend),
    );
}
