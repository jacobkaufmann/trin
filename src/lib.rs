use std::{path::PathBuf, str::FromStr, sync::Arc};

use tokio::sync::{mpsc, RwLock};
use tracing::debug;

use trin_core::{
    cli::{TrinConfig, HISTORY_NETWORK, STATE_NETWORK},
    jsonrpc::{
        handlers::JsonRpcHandler,
        service::{launch_jsonrpc_server, JsonRpcExiter},
        types::PortalJsonRpcRequest,
    },
    portalnet::{
        discovery::Discovery, events::PortalnetEvents, storage::PortalStorageConfig,
        types::messages::PortalnetConfig,
    },
    types::{accumulator::MasterAccumulator, validation::HeaderOracle},
    utils::{bootnodes::parse_bootnodes, db, provider::TrustedProvider},
    utp::stream::UtpListener,
};
use trin_history::initialize_history_network;
use trin_state::initialize_state_network;

/// Environment variable for path to data directory.
const TRIN_DATA_ENV_VAR: &str = "TRIN_DATA_PATH";

pub async fn run_trin(
    trin_config: TrinConfig,
    trusted_provider: TrustedProvider,
) -> Result<Arc<JsonRpcExiter>, Box<dyn std::error::Error>> {
    trin_config.display_config();

    let bootnode_enrs = parse_bootnodes(&trin_config.bootnodes)?;
    let portalnet_config = PortalnetConfig {
        external_addr: trin_config.external_addr,
        private_key: trin_config.private_key.clone(),
        listen_port: trin_config.discovery_port,
        no_stun: trin_config.no_stun,
        enable_metrics: trin_config.enable_metrics_with_url.is_some(),
        bootnode_enrs,
        ..Default::default()
    };

    // Initialize base discovery protocol
    let mut discovery = Discovery::new(portalnet_config.clone()).unwrap();
    let talk_req_rx = discovery.start().await.unwrap();
    let discovery = Arc::new(discovery);
    let local_node_id = discovery.local_enr().node_id();

    // Initialize prometheus metrics
    if let Some(addr) = trin_config.enable_metrics_with_url {
        prometheus_exporter::start(addr).unwrap();
    }

    // Initialize and spawn UTP listener
    let (utp_events_tx, utp_listener_tx, utp_listener_rx, mut utp_listener) =
        UtpListener::new(Arc::clone(&discovery));
    tokio::spawn(async move { utp_listener.start().await });

    // Initialize Storage config
    let data_dir = if trin_config.ephemeral {
        // If ephemeral, then use a temporary directory.
        let temp_dir_path = db::temp_dir_path();
        let path = db::data_dir(local_node_id, Some(temp_dir_path)).unwrap();
        tempfile::TempDir::new_in(&path)?;
        path
    } else {
        // If not ephemeral, then check the environment for a data directory.
        let env_data_path = std::env::var(TRIN_DATA_ENV_VAR).ok();
        let env_data_path = env_data_path.and_then(|path| PathBuf::from_str(path.as_str()).ok());
        let path = db::data_dir(local_node_id, env_data_path).unwrap();
        std::fs::create_dir_all(&path)?;
        path
    };

    let storage_config = PortalStorageConfig::new(
        trin_config.kb.into(),
        data_dir,
        discovery.local_enr().node_id(),
        trin_config.enable_metrics_with_url.is_some(),
    );

    // Initialize validation oracle
    let master_accumulator = MasterAccumulator::try_from_file(trin_config.master_acc_path.clone())?;
    let header_oracle = HeaderOracle::new(trusted_provider.clone(), master_accumulator);
    let header_oracle = Arc::new(RwLock::new(header_oracle));

    debug!("Selected networks to spawn: {:?}", trin_config.networks);
    // Initialize state sub-network service and event handlers, if selected
    let (state_handler, state_network_task, state_event_tx, state_utp_tx, state_jsonrpc_tx) =
        if trin_config.networks.iter().any(|val| val == STATE_NETWORK) {
            initialize_state_network(
                &discovery,
                utp_listener_tx.clone(),
                portalnet_config.clone(),
                storage_config.clone(),
                header_oracle.clone(),
            )
            .await
        } else {
            (None, None, None, None, None)
        };

    // Initialize chain history sub-network service and event handlers, if selected
    let (
        history_handler,
        history_network_task,
        history_event_tx,
        history_utp_tx,
        history_jsonrpc_tx,
    ) = if trin_config
        .networks
        .iter()
        .any(|val| val == HISTORY_NETWORK)
    {
        initialize_history_network(
            &discovery,
            utp_listener_tx,
            portalnet_config.clone(),
            storage_config.clone(),
            header_oracle.clone(),
        )
        .await
    } else {
        (None, None, None, None, None)
    };

    // Initialize json-rpc server
    let (portal_jsonrpc_tx, portal_jsonrpc_rx) = mpsc::unbounded_channel::<PortalJsonRpcRequest>();
    let jsonrpc_trin_config = trin_config.clone();
    let (live_server_tx, mut live_server_rx) = tokio::sync::mpsc::channel::<bool>(1);
    let json_exiter = Arc::new(JsonRpcExiter::new());
    {
        let json_exiter_clone = Arc::clone(&json_exiter);
        tokio::task::spawn_blocking(|| {
            launch_jsonrpc_server(
                jsonrpc_trin_config,
                trusted_provider,
                portal_jsonrpc_tx,
                live_server_tx,
                json_exiter_clone,
            );
        });
    }

    // Spawn main JsonRpc Handler
    let jsonrpc_discovery = Arc::clone(&discovery);
    let rpc_handler = JsonRpcHandler {
        discovery: jsonrpc_discovery,
        portal_jsonrpc_rx,
        state_jsonrpc_tx,
        history_jsonrpc_tx,
    };

    tokio::spawn(rpc_handler.process_jsonrpc_requests());

    if let Some(handler) = state_handler {
        tokio::spawn(handler.handle_client_queries());
    }
    if let Some(handler) = history_handler {
        tokio::spawn(handler.handle_client_queries());
    }

    // Spawn main portal events handler
    tokio::spawn(async move {
        let events = PortalnetEvents::new(
            talk_req_rx,
            utp_listener_rx,
            history_event_tx,
            history_utp_tx,
            state_event_tx,
            state_utp_tx,
            utp_events_tx,
        )
        .await;
        events.start().await;
    });

    if let Some(network) = history_network_task {
        tokio::spawn(async { network.await });
    }
    if let Some(network) = state_network_task {
        tokio::spawn(async { network.await });
    }

    let _ = live_server_rx.recv().await;
    live_server_rx.close();

    Ok(json_exiter)
}
