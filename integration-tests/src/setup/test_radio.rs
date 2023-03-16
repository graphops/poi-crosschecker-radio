#![allow(clippy::await_holding_lock)]
use crate::utils::{
    empty_attestation_handler, generate_random_address, get_random_port, setup_mock_env_vars,
    setup_mock_server, RadioTestConfig,
};
use chrono::Utc;

use ethers::signers::LocalWallet;
use ethers_contract::EthAbiType;
use ethers_core::types::transaction::eip712::Eip712;
use ethers_derive_eip712::*;
use graphcast_sdk::config::NetworkName;
use graphcast_sdk::graphcast_agent::message_typing::GraphcastMessage;
use graphcast_sdk::graphcast_agent::GraphcastAgent;
use graphcast_sdk::graphql::client_graph_node::update_chainhead_blocks;
use graphcast_sdk::graphql::client_network::query_network_subgraph;
use graphcast_sdk::graphql::client_registry::query_registry_indexer;
use graphcast_sdk::{
    comparison_trigger, determine_message_block, graphcast_id_address, BlockPointer,
};
use hex::encode;
use partial_application::partial;
use poi_radio::{
    attestation_handler, chainhead_block_str, compare_attestations, process_messages,
    save_local_attestation, Attestation, ComparisonResult, LocalAttestationsMap, MessagesArc,
    RadioPayloadMessage, RemoteAttestationsMap, GRAPHCAST_AGENT, MESSAGES,
};
use prost::Message;
use rand::{thread_rng, Rng};
use secp256k1::SecretKey;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::env;
use std::sync::{Arc, Mutex as SyncMutex};
use std::{thread::sleep, time::Duration};
use tokio::sync::Mutex as AsyncMutex;
use tracing::{debug, error, info, trace};

use crate::setup::constants::{MOCK_SUBGRAPH_GOERLI, MOCK_SUBGRAPH_MAINNET};
use poi_radio::graphql::query_graph_node_poi;

fn round_to_nearest(number: i64) -> i64 {
    (number / 10) * 10 + if number % 10 > 4 { 10 } else { 0 }
}

#[derive(Eip712, EthAbiType, Clone, Message, Serialize, Deserialize)]
#[eip712(
    name = "Graphcast POI Radio Dummy Msg",
    version = "0",
    chain_id = 1,
    verifying_contract = "0xc944e90c64b2c07662a292be6244bdf05cda44a7"
)]
pub struct DummyMsg {
    #[prost(string, tag = "1")]
    pub identifier: String,
    #[prost(int32, tag = "2")]
    pub dummy_value: i32,
}

impl DummyMsg {
    pub fn new(identifier: String, dummy_value: i32) -> Self {
        DummyMsg {
            identifier,
            dummy_value,
        }
    }
}

pub async fn run_test_radio<S, A, P>(
    runtime_config: &RadioTestConfig,
    success_handler: S,
    test_attestation_handler: A,
    post_comparison_handler: P,
) where
    S: Fn(MessagesArc),
    A: Fn(u64, &RemoteAttestationsMap, &LocalAttestationsMap),
    P: Fn(MessagesArc, u64, &str, usize),
{
    let collect_message_duration: i64 = env::var("COLLECT_MESSAGE_DURATION")
        .unwrap_or("1".to_string())
        .parse::<i64>()
        .unwrap_or(1);

    let indexer_address = runtime_config
        .indexer_address
        .clone()
        .unwrap_or(generate_random_address());

    let graphcast_id = runtime_config
        .operator_address
        .clone()
        .unwrap_or(generate_random_address());

    debug!("Actual graphcast_id: {}", graphcast_id);

    let mock_server_uri = setup_mock_server(
        round_to_nearest(Utc::now().timestamp()).try_into().unwrap(),
        &indexer_address,
        &graphcast_id,
        &runtime_config.subgraphs.clone().unwrap_or(vec![
            MOCK_SUBGRAPH_MAINNET.to_string(),
            MOCK_SUBGRAPH_GOERLI.to_string(),
        ]),
        &runtime_config.indexer_stake,
        &runtime_config.poi,
    )
    .await;
    setup_mock_env_vars(&mock_server_uri);

    let private_key = env::var("PRIVATE_KEY").expect("No private key provided.");
    let registry_subgraph =
        env::var("REGISTRY_SUBGRAPH_ENDPOINT").expect("No registry subgraph endpoint provided.");
    let network_subgraph =
        env::var("NETWORK_SUBGRAPH_ENDPOINT").expect("No network subgraph endpoint provided.");
    let graph_node_endpoint =
        env::var("GRAPH_NODE_STATUS_ENDPOINT").expect("No Graph node status endpoint provided.");

    let wallet = private_key.parse::<LocalWallet>().unwrap();
    let mut rng = thread_rng();
    let mut private_key = [0u8; 32];
    rng.fill(&mut private_key[..]);

    let private_key = SecretKey::from_slice(&private_key).expect("Error parsing secret key");
    let private_key_hex = encode(private_key.secret_bytes());
    env::set_var("PRIVATE_KEY", &private_key_hex);

    let private_key = env::var("PRIVATE_KEY").unwrap();

    // TODO: Add something random and unique here to avoid noise form other operators
    let radio_name: &str = "test-poi-radio";

    let my_address =
        query_registry_indexer(registry_subgraph.clone(), graphcast_id_address(&wallet))
            .await
            .unwrap();
    let my_stake = query_network_subgraph(network_subgraph.clone(), my_address.clone())
        .await
        .unwrap()
        .indexer_stake();
    info!(
        "Initializing radio to act on behalf of indexer {:#?} with stake {}",
        my_address.clone(),
        my_stake
    );

    let graphcast_agent = GraphcastAgent::new(
        private_key,
        radio_name,
        &registry_subgraph,
        &network_subgraph,
        &graph_node_endpoint,
        vec![],
        Some("testnet"),
        runtime_config.subgraphs.clone().unwrap_or(vec![
            MOCK_SUBGRAPH_MAINNET.to_string(),
            MOCK_SUBGRAPH_GOERLI.to_string(),
        ]),
        None,
        None,
        Some(get_random_port()),
        None,
    )
    .await
    .unwrap();

    _ = GRAPHCAST_AGENT.set(graphcast_agent);
    _ = MESSAGES.set(Arc::new(SyncMutex::new(vec![])));

    if runtime_config.is_setup_instance {
        GRAPHCAST_AGENT
            .get()
            .unwrap()
            .register_handler(Arc::new(AsyncMutex::new(empty_attestation_handler())))
            .expect("Could not register handler");
    } else {
        GRAPHCAST_AGENT
            .get()
            .unwrap()
            .register_handler(Arc::new(AsyncMutex::new(attestation_handler())))
            .expect("Could not register handler");
    };

    let mut network_chainhead_blocks: HashMap<NetworkName, BlockPointer> = HashMap::new();
    let local_attestations: Arc<AsyncMutex<LocalAttestationsMap>> =
        Arc::new(AsyncMutex::new(HashMap::new()));

    // Main loop for sending messages, can factor out
    // and take radio specific query and parsing for radioPayload
    loop {
        let subgraph_network_latest_blocks = match update_chainhead_blocks(
            graph_node_endpoint.clone(),
            &mut network_chainhead_blocks,
        )
        .await
        {
            Ok(res) => res,
            Err(e) => {
                error!("Could not query indexing statuses, pull again later: {e}");
                continue;
            }
        };

        debug!(
            "Subgraph network and latest blocks: {:#?}",
            subgraph_network_latest_blocks,
        );
        let identifiers = GRAPHCAST_AGENT.get().unwrap().content_identifiers().await;
        let num_topics = identifiers.len();
        //TODO: move to helper
        let blocks_str = chainhead_block_str(&network_chainhead_blocks);
        info!(
            "Network statuses:\n{}: {:#?}\n{}: {:#?}\n{}: {}",
            "Chainhead blocks",
            blocks_str,
            "Number of gossip peers",
            GRAPHCAST_AGENT.get().unwrap().number_of_peers(),
            "Number of tracked deployments (topics)",
            num_topics,
        );

        for id in identifiers {
            // Get the indexing network of the deployment
            // and update the NETWORK message block
            let (network_name, latest_block) = match subgraph_network_latest_blocks.get(&id.clone())
            {
                Some(network_block) => (
                    NetworkName::from_string(&network_block.network.clone()),
                    network_block.block.clone(),
                ),
                None => {
                    error!("Could not query the subgraph's indexing network, check Graph node's indexing statuses of subgraph deployment {}", id.clone());
                    continue;
                }
            };

            let message_block =
                match determine_message_block(&network_chainhead_blocks, network_name) {
                    Ok(block) => block,
                    Err(_) => continue,
                };

            // first stored message block
            let (compare_block, comparison_trigger) = comparison_trigger(
                Arc::new(AsyncMutex::new(
                    MESSAGES.get().unwrap().lock().unwrap().to_vec(),
                )),
                id.clone(),
                collect_message_duration,
            )
            .await;

            info!(
                "Deployment status:\n{}: {}\n{}: {}\n{}: {}\n{}: {}\n{}: {}\n{}: {}",
                "IPFS Hash",
                id.clone(),
                "Network",
                network_name,
                "Send message block",
                message_block,
                "Latest block",
                latest_block.number,
                "Reached send message block",
                latest_block.number >= message_block,
                "Reached comparison time",
                Utc::now().timestamp() >= comparison_trigger,
            );

            if Utc::now().timestamp() >= comparison_trigger {
                debug!("{}", "Comparing attestations");
                trace!("{}{:?}", "Messages: ", MESSAGES);

                let msgs: Vec<GraphcastMessage<RadioPayloadMessage>> = MESSAGES
                    .get()
                    .unwrap()
                    .lock()
                    .unwrap()
                    .to_vec()
                    .iter()
                    .filter(|&m| m.identifier == id.clone() && m.block_number == compare_block)
                    .cloned()
                    .collect();

                debug!(
                    "Comparing validated messages:\n{}: {}\n{}: {}\n{}: {}",
                    "Deployment",
                    id.clone(),
                    "Block",
                    compare_block,
                    "Number of messages",
                    msgs.len(),
                );
                let remote_attestations_result = process_messages(
                    Arc::new(AsyncMutex::new(msgs)),
                    &registry_subgraph,
                    &network_subgraph,
                )
                .await;

                let remote_attestations = match remote_attestations_result {
                    Ok(remote) => {
                        success_handler(Arc::clone(MESSAGES.get().unwrap()));

                        test_attestation_handler(
                            compare_block,
                            &remote,
                            &local_attestations.lock().await.clone(),
                        );

                        debug!(
                            "Processed messages:\n{}: {}",
                            "Number of unique remote POIs",
                            remote.len(),
                        );
                        remote
                    }
                    Err(err) => {
                        error!("{}{}", "An error occured while parsing messages: {}", err);
                        continue;
                    }
                };

                let comparison_result = compare_attestations(
                    compare_block,
                    remote_attestations.clone(),
                    Arc::clone(&local_attestations),
                )
                .await;

                match comparison_result {
                    Ok(ComparisonResult::Match(msg)) => {
                        debug!("{}", msg);
                        let len = MESSAGES.get().unwrap().lock().unwrap().to_vec().len();
                        MESSAGES.get().unwrap().lock().unwrap().retain(|msg| {
                            msg.block_number != compare_block || msg.identifier != id.clone()
                        });
                        debug!("Messages left: {:#?}", MESSAGES);
                        post_comparison_handler(
                            Arc::clone(MESSAGES.get().unwrap()),
                            compare_block,
                            &id,
                            len,
                        );
                    }
                    Ok(ComparisonResult::Divergent(err)) => {
                        if runtime_config.panic_if_poi_diverged {
                            panic!("{}", err);
                        } else {
                            let len = MESSAGES.get().unwrap().lock().unwrap().to_vec().len();
                            MESSAGES.get().unwrap().lock().unwrap().retain(|msg| {
                                msg.block_number != compare_block || msg.identifier != id.clone()
                            });
                            debug!("Messages left: {:#?}", MESSAGES);
                            error!("{}", err);
                            post_comparison_handler(
                                Arc::clone(MESSAGES.get().unwrap()),
                                compare_block,
                                &id,
                                len,
                            );
                        }
                    }
                    Ok(ComparisonResult::NotFound(msg)) => {
                        info!("Not found: {}", msg);
                    }

                    Err(err) => {
                        error!("{}{}", "An error occured while parsing messages: {}", err);
                    }
                }
            }

            let poi_query =
                partial!( query_graph_node_poi => graph_node_endpoint.clone(), id.clone(), _, _);

            debug!(
                "Checking latest block number and the message block: {0} >?= {message_block}",
                latest_block.number
            );
            if latest_block.number >= message_block {
                let block_hash = match GRAPHCAST_AGENT
                    .get()
                    .unwrap()
                    .get_block_hash(network_name.to_string(), message_block)
                    .await
                {
                    Ok(hash) => hash,
                    Err(e) => {
                        error!("Failed to query graph node for the block hash: {e}");
                        continue;
                    }
                };

                if runtime_config.invalid_payload {
                    // Send dummy msg
                    debug!("Sending dummy message");
                    let radio_message = DummyMsg::new(id.clone(), 5);
                    info!("{}: {:?}", "Attempting to send message", radio_message);

                    match GRAPHCAST_AGENT
                        .get()
                        .unwrap()
                        .send_message(id.clone(), network_name, message_block, Some(radio_message))
                        .await
                    {
                        Ok(sent) => {
                            info!("{}: {}", "Sent message id", sent);
                        }
                        Err(e) => error!("{}: {}", "Failed to send message", e),
                    };

                    continue;
                }

                match poi_query(block_hash.clone(), message_block.try_into().unwrap()).await {
                    Ok(content) => {
                        let attestation = Attestation {
                            npoi: content.clone(),
                            stake_weight: my_stake.clone(),
                            senders: Vec::new(),
                        };

                        save_local_attestation(
                            &mut *local_attestations.lock().await,
                            attestation,
                            id.clone(),
                            message_block,
                        );

                        let radio_message = RadioPayloadMessage::new(id.clone(), content.clone());
                        info!("{}: {:?}", "Attempting to send message", radio_message);

                        match GRAPHCAST_AGENT
                            .get()
                            .unwrap()
                            .send_message(
                                id.clone(),
                                network_name,
                                message_block,
                                Some(radio_message),
                            )
                            .await
                        {
                            Ok(sent) => {
                                info!("{}: {}", "Sent message id", sent);
                            }
                            Err(e) => error!("{}: {}", "Failed to send message", e),
                        };
                    }
                    Err(e) => error!("{}: {}", "Failed to query message", e),
                }
            }
        }
        setup_mock_server(
            round_to_nearest(Utc::now().timestamp()).try_into().unwrap(),
            &indexer_address,
            &graphcast_id,
            &runtime_config.subgraphs.clone().unwrap_or(vec![
                MOCK_SUBGRAPH_MAINNET.to_string(),
                MOCK_SUBGRAPH_GOERLI.to_string(),
            ]),
            &runtime_config.indexer_stake,
            &runtime_config.poi,
        )
        .await;
        sleep(Duration::from_secs(5));
        continue;
    }
}
