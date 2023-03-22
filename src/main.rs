use chrono::Utc;

use dotenv::dotenv;
use ethers::signers::LocalWallet;
use graphcast_sdk::bots::{DiscordBot, SlackBot};
/// Radio specific query function to fetch Proof of Indexing for each allocated subgraph
use graphcast_sdk::config::Config;
use graphcast_sdk::graphcast_agent::message_typing::GraphcastMessage;
use graphcast_sdk::graphcast_agent::GraphcastAgent;
use graphcast_sdk::graphql::client_graph_node::update_chainhead_blocks;
use graphcast_sdk::graphql::client_network::query_network_subgraph;
use graphcast_sdk::graphql::client_registry::query_registry_indexer;
use graphcast_sdk::networks::NetworkName;
use graphcast_sdk::{
    comparison_trigger, determine_message_block, graphcast_id_address, BlockPointer,
};

use poi_radio::{
    attestation_handler, chainhead_block_str, compare_attestations, generate_topics,
    metrics::start_prometheus_server, process_messages, save_local_attestation, Attestation,
    ComparisonResult, LocalAttestationsMap, RadioPayloadMessage, GRAPHCAST_AGENT, MESSAGES,
};
use std::collections::HashMap;
use std::sync::{Arc, Mutex as SyncMutex};
use std::{thread::sleep, time::Duration};
use tokio::sync::Mutex as AsyncMutex;
use tracing::log::warn;
use tracing::{debug, error, info};

use crate::graphql::query_graph_node_poi;

mod graphql;

#[macro_use]
extern crate partial_application;

#[tokio::main]
async fn main() {
    let radio_name: &str = "poi-radio";
    dotenv().ok();

    // Parse basic configurations
    let config = Config::args();
    if let Err(e) = config.validate_set_up().await {
        panic!("Could not validate the supplied configurations: {e}")
    }

    if let Some(true) = config.prometheus_metrics {
        start_prometheus_server();
    }

    let graph_node_endpoint = config.graph_node_endpoint.clone();
    let private_key = &config.wallet_input().unwrap().to_string();
    let wallet = private_key.parse::<LocalWallet>().unwrap();

    // Using unwrap directly as the query has been ran in the set-up validation
    // The query here must be Ok but so it is okay to panic here
    // Alternatively, make validate_set_up to return these two fields
    let my_address = query_registry_indexer(
        config.registry_subgraph.to_string(),
        graphcast_id_address(&wallet),
    )
    .await
    .unwrap();
    let my_stake = query_network_subgraph(config.network_subgraph.to_string(), my_address.clone())
        .await
        .unwrap()
        .indexer_stake();
    info!(
        "Initializing radio to act on behalf of indexer {:#?} with stake {}",
        my_address.clone(),
        my_stake
    );

    let generate_topics = partial!(generate_topics => config.network_subgraph.clone(), my_address.clone(), &config.topics);
    let topics = generate_topics().await;
    info!("Found content topics for subscription: {:?}", topics);

    debug!("Initializing the Graphcast Agent");
    let graphcast_agent = GraphcastAgent::new(
        config.private_key.clone().unwrap().clone(),
        radio_name,
        &config.registry_subgraph,
        &config.network_subgraph,
        &graph_node_endpoint,
        config.boot_node_addresses.clone(),
        Some(&config.graphcast_network),
        topics,
        // Maybe move Waku specific configs to a sub-group
        config.waku_node_key.clone(),
        config.waku_host.clone(),
        config.waku_port.clone(),
        None,
    )
    .await
    .expect("Initialize Graphcast agent");

    _ = GRAPHCAST_AGENT.set(graphcast_agent);
    _ = MESSAGES.set(Arc::new(SyncMutex::new(vec![])));

    GRAPHCAST_AGENT
        .get()
        .unwrap()
        .register_handler(Arc::new(AsyncMutex::new(attestation_handler())))
        .expect("Could not register handler");

    let mut network_chainhead_blocks: HashMap<NetworkName, BlockPointer> = HashMap::new();
    let local_attestations: Arc<AsyncMutex<LocalAttestationsMap>> =
        Arc::new(AsyncMutex::new(HashMap::new()));

    // Main loop for sending messages, can factor out
    // and take radio specific query and parsing for radioPayload
    loop {
        // Update topic subscription
        if Utc::now().timestamp() % 120 == 0 {
            GRAPHCAST_AGENT
                .get()
                .unwrap()
                .update_content_topics(generate_topics().await)
                .await;
        }
        // Update all the chainheads of the network
        // Also get a hash map returned on the subgraph mapped to network name and latest block
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
        //TODO: check that if no networks had an new message update blocks, sleep for a few seconds and 'continue'

        // Radio specific message content query function
        // Function takes in an identifier string and make specific queries regarding the identifier
        // The example here combines a single function provided query endpoint, current block info based on the subgraph's indexing network
        // Then the function gets sent to agent for making identifier independent queries
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
        let mut messages_sent = vec![];
        let mut comparison_result_strings = vec![];
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

            let msgs = MESSAGES.get().unwrap().lock().unwrap().to_vec();
            // first stored message block
            let (compare_block, comparison_trigger) = comparison_trigger(
                Arc::new(AsyncMutex::new(msgs)),
                id.clone(),
                config.collect_message_duration,
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
                let msgs = MESSAGES.get().unwrap().lock().unwrap().to_vec();
                // Update to only process the identifier&compare_block related messages
                let msgs: Vec<GraphcastMessage<RadioPayloadMessage>> = msgs
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
                    &config.registry_subgraph,
                    &config.network_subgraph,
                )
                .await;
                let remote_attestations = match remote_attestations_result {
                    Ok(remote) => {
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
                    network_name,
                    compare_block,
                    remote_attestations.clone(),
                    Arc::clone(&local_attestations),
                    id.clone(),
                )
                .await;

                match comparison_result {
                    Ok(ComparisonResult::Match(msg)) => {
                        debug!("{}", msg.clone());
                        comparison_result_strings.push(ComparisonResult::Match(msg.clone()));
                        // Only clear the ones matching identifier and block number
                        MESSAGES.get().unwrap().lock().unwrap().retain(|msg| {
                            msg.block_number != compare_block || msg.identifier != id.clone()
                        });
                        debug!("Messages left: {:#?}", MESSAGES);
                    }
                    Ok(ComparisonResult::NotFound(msg)) => {
                        warn!("{}", msg);
                        comparison_result_strings.push(ComparisonResult::NotFound(msg.clone()));
                        // TODO: perhaps add conditional remove by timestamps
                        MESSAGES.get().unwrap().lock().unwrap().retain(|msg| {
                            msg.block_number != compare_block || msg.identifier != id.clone()
                        });
                        debug!("Messages left: {:#?}", MESSAGES);
                    }
                    Ok(ComparisonResult::Divergent(msg)) => {
                        error!("{}", msg);

                        if let (Some(token), Some(channel)) =
                            (&config.slack_token, &config.slack_channel)
                        {
                            if let Err(e) = SlackBot::send_webhook(
                                token.to_string(),
                                channel.as_str(),
                                radio_name,
                                msg.as_str(),
                            )
                            .await
                            {
                                warn!("Failed to send notification to Slack: {}", e);
                            }
                        }

                        if let Some(webhook_url) = &config.discord_webhook {
                            if let Err(e) =
                                DiscordBot::send_webhook(webhook_url, radio_name, msg.as_str())
                                    .await
                            {
                                warn!("Failed to send notification to Discord: {}", e);
                            }
                        }

                        comparison_result_strings.push(ComparisonResult::Divergent(msg.clone()));
                        // TODO: perhaps add conditional remove by timestamps
                        MESSAGES.get().unwrap().lock().unwrap().retain(|msg| {
                            msg.block_number != compare_block || msg.identifier != id.clone()
                        });
                        debug!("Messages left: {:#?}", MESSAGES);
                    }
                    Err(e) => {
                        error!("An error occured while comparing attestations: {}", e);
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
                            Ok(id) => messages_sent.push(id),
                            Err(e) => error!("{}: {}", "Failed to send message", e),
                        };
                    }
                    Err(e) => error!("{}: {}", "Failed to query message content", e),
                }
            }
        }

        // Generate attestation summary
        let mut match_strings = vec![];
        let mut not_found_strings = vec![];
        let mut divergent_strings = vec![];

        for result in comparison_result_strings {
            match result {
                ComparisonResult::Match(s) => {
                    match_strings.push(s);
                }
                ComparisonResult::NotFound(s) => {
                    not_found_strings.push(s);
                }
                ComparisonResult::Divergent(s) => {
                    divergent_strings.push(s);
                }
            }
        }
        info!(
            "Operation summary:\n{}: {}\n{} out of {} deployments cross checked\n{}: {}\n{}: {}\n{}: {:#?}",
            "Number of messages sent",
            messages_sent.len(),
            match_strings.len() + divergent_strings.len(),
            num_topics,
            "Successful attestations",
            match_strings.len(),
            "Topics without attestations",
            not_found_strings.len(),
            "Divergence",
            divergent_strings,
        );
        sleep(Duration::from_secs(5));
        continue;
    }
}
