use async_graphql::{
    Context, EmptyMutation, EmptySubscription, InputObject, Object, Schema, SimpleObject,
};
use std::sync::{Arc, Mutex as SyncMutex};
use tokio::sync::Mutex as AsyncMutex;
use tracing::debug;

use crate::{
    config::Config,
    operator::attestation::{
        attestations_to_vec, compare_attestations, process_messages, Attestation, AttestationEntry,
        AttestationError, ComparisonResult, ComparisonResultType, LocalAttestationsMap,
    },
    state::PersistedState,
    RadioPayloadMessage,
};
use graphcast_sdk::graphcast_agent::message_typing::GraphcastMessage;

pub(crate) type POIRadioSchema = Schema<QueryRoot, EmptyMutation, EmptySubscription>;

// Unified query object for resolvers
#[derive(Default)]
pub struct QueryRoot;

#[Object]
impl QueryRoot {
    async fn radio_payload_messages(
        &self,
        ctx: &Context<'_>,
    ) -> Result<Vec<GraphcastMessage<RadioPayloadMessage>>, anyhow::Error> {
        let state = ctx
            .data_unchecked::<Arc<SyncMutex<PersistedState>>>()
            .lock()
            .unwrap()
            .clone();
        Ok(state.remote_messages().lock().unwrap().clone())
    }

    async fn radio_payload_messages_by_deployment(
        &self,
        ctx: &Context<'_>,
        identifier: String,
    ) -> Result<Vec<GraphcastMessage<RadioPayloadMessage>>, anyhow::Error> {
        let state = ctx
            .data_unchecked::<Arc<SyncMutex<PersistedState>>>()
            .lock()
            .unwrap()
            .clone();
        let msg = state.remote_messages().lock().unwrap().clone();
        Ok(msg
            .iter()
            .cloned()
            .filter(|message| message.identifier == identifier.clone())
            .collect::<Vec<_>>())
    }

    async fn local_attestations(
        &self,
        ctx: &Context<'_>,
        identifier: Option<String>,
        block: Option<u64>,
    ) -> Result<Vec<AttestationEntry>, anyhow::Error> {
        let state = ctx
            .data_unchecked::<Arc<SyncMutex<PersistedState>>>()
            .lock()
            .unwrap()
            .clone();
        let attestations = &state.local_attestations();
        let filtered = attestations_to_vec(attestations)
            .into_iter()
            .filter(|entry| filter_attestations(entry, &identifier, &block))
            .collect::<Vec<_>>();

        Ok(filtered)
    }

    // TODO: Reproduce tabular summary view. use process_message and compare_attestations
    async fn comparison_results(
        &self,
        ctx: &Context<'_>,
        deployment: Option<String>,
        block: Option<u64>,
        filter: Option<ResultFilter>,
    ) -> Result<Vec<ComparisonResult>, anyhow::Error> {
        // Utilize the provided filters on local_attestations
        let locals: Vec<AttestationEntry> = match self
            .local_attestations(ctx, deployment.clone(), block)
            .await
        {
            Ok(r) => r,
            Err(e) => return Err(e),
        }
        .into_iter()
        .filter(|entry| filter_attestations(entry, &deployment.clone(), &block))
        .collect::<Vec<AttestationEntry>>();

        let mut res = vec![];
        for entry in locals {
            let r = self
                .comparison_result(ctx, entry.deployment, entry.block_number)
                .await;
            // ignore errored comparison for now
            if r.is_err() {
                continue;
            }
            let result = r.unwrap();
            if filter_results(&result, &filter) {
                res.push(result);
            }
        }

        Ok(res)
    }

    async fn comparison_result(
        &self,
        ctx: &Context<'_>,
        deployment: String,
        block: u64,
    ) -> Result<ComparisonResult, AttestationError> {
        let state = ctx
            .data_unchecked::<Arc<AsyncMutex<PersistedState>>>()
            .lock()
            .await
            .clone();
        let msgs = state.remote_messages().lock().unwrap().clone();
        let local_attestations = &state.local_attestations();
        let config = ctx.data_unchecked::<Config>();
        let filter_msg: Vec<GraphcastMessage<RadioPayloadMessage>> = msgs
            .iter()
            .filter(|&m| m.block_number == block)
            .cloned()
            .collect();

        let registry_subgraph = config.registry_subgraph.clone();
        let network_subgraph = config.network_subgraph.clone();
        let remote_attestations_result =
            process_messages(filter_msg, &registry_subgraph, &network_subgraph).await;
        let remote_attestations = match remote_attestations_result {
            Ok(remote) => {
                debug!(
                    number_of_unique_remote_npois = remote.len(),
                    "Processed messages",
                );

                remote
            }
            Err(err) => {
                debug!(
                    err = tracing::field::debug(&err),
                    "An error occured while parsing messages"
                );
                return Err(err);
            }
        };
        let comparison_result = compare_attestations(
            block,
            remote_attestations,
            Arc::clone(local_attestations),
            &deployment.clone(),
        )
        .await;

        Ok(comparison_result)
    }

    /// Return the sender ratio for remote attestations, with a "!" for the attestation matching local
    async fn sender_ratio(
        &self,
        ctx: &Context<'_>,
        deployment: Option<String>,
        block: Option<u64>,
        filter: Option<ResultFilter>,
    ) -> Result<Vec<CompareRatio>, anyhow::Error> {
        let res = self
            .comparison_results(ctx, deployment, block, filter)
            .await?;
        let mut ratios = vec![];
        for r in res {
            let ratio =
                sender_count_str(&r.attestations, r.local_attestation.unwrap().npoi.clone());
            ratios.push(CompareRatio::new(r.deployment, r.block_number, ratio));
        }
        Ok(ratios)
    }

    /// Return the stake weight for remote attestations, with a "!" for the attestation matching local
    async fn stake_ratio(
        &self,
        ctx: &Context<'_>,
        deployment: Option<String>,
        block: Option<u64>,
        filter: Option<ResultFilter>,
    ) -> Result<Vec<CompareRatio>, anyhow::Error> {
        let res = self
            .comparison_results(ctx, deployment, block, filter)
            .await?;
        let mut ratios = vec![];
        for r in res {
            let ratio =
                stake_weight_str(&r.attestations, r.local_attestation.unwrap().npoi.clone());
            ratios.push(CompareRatio::new(r.deployment, r.block_number, ratio));
        }
        Ok(ratios)
    }
}

/// Helper function to order attestations by stake weight and then find the number of unique senders
pub fn sender_count_str(attestations: &[Attestation], local_npoi: String) -> String {
    // Create a HashMap to store the attestation and senders
    let mut temp_attestations = attestations.to_owned();
    let mut output = String::new();

    // Sort the attestations by descending stake weight
    temp_attestations.sort_by(|a, b| b.stake_weight.cmp(&a.stake_weight));
    // Iterate through the attestations and populate the maps
    // No set is needed since uniqueness is garuanteeded by validation
    for att in attestations.iter() {
        let separator = if att.npoi == local_npoi { "!/" } else { "/" };

        output.push_str(&format!("{}{}", att.senders.len(), separator));
    }

    output.pop(); // Remove the trailing '/'

    output
}

/// Helper function to order attestations by stake weight and then find the number of unique senders
pub fn stake_weight_str(attestations: &[Attestation], local_npoi: String) -> String {
    // Create a HashMap to store the attestation and senders
    let mut temp_attestations = attestations.to_owned();
    let mut output = String::new();

    // Sort the attestations by descending stake weight
    temp_attestations.sort_by(|a, b| b.stake_weight.cmp(&a.stake_weight));
    // Iterate through the attestations and populate the maps
    // No set is needed since uniqueness is garuanteeded by validation
    for att in attestations.iter() {
        let separator = if att.npoi == local_npoi { "!/" } else { "/" };
        output.push_str(&format!("{}{}", att.stake_weight, separator));
    }

    output.pop(); // Remove the trailing '/'

    output
}

pub async fn build_schema(ctx: Arc<POIRadioContext>) -> POIRadioSchema {
    Schema::build(QueryRoot, EmptyMutation, EmptySubscription)
        .data(Arc::clone(&ctx.persisted_state))
        .finish()
}

pub struct POIRadioContext {
    pub radio_config: Config,
    pub persisted_state: Arc<SyncMutex<PersistedState>>,
}

impl POIRadioContext {
    pub async fn init(
        radio_config: Config,
        persisted_state: Arc<SyncMutex<PersistedState>>,
    ) -> Self {
        Self {
            radio_config,
            persisted_state,
        }
    }

    pub async fn local_attestations(&self) -> LocalAttestationsMap {
        self.persisted_state
            .lock()
            .unwrap()
            .local_attestations()
            .lock()
            .unwrap()
            .clone()
    }
}

/// Filter funciton for Attestations on deployment and block
fn filter_attestations(
    entry: &AttestationEntry,
    identifier: &Option<String>,
    block: &Option<u64>,
) -> bool {
    let is_matching_deployment = match identifier {
        Some(dep) => entry.deployment == dep.clone(),
        None => true, // Skip check
    };
    let is_matching_block = match block {
        Some(b) => entry.block_number == *b,
        None => true, // Skip check
    };
    is_matching_deployment && is_matching_block
}

fn filter_results(entry: &ComparisonResult, filter: &Option<ResultFilter>) -> bool {
    let (identifier, block, result): (Option<String>, Option<u64>, Option<ComparisonResultType>) =
        match filter {
            None => (None, None, None),
            Some(f) => (f.deployment.clone(), f.block_number, f.result_type),
        };

    let is_matching_deployment = match identifier {
        Some(dep) => entry.deployment == dep,
        None => true, // Skip check
    };
    let is_matching_block = match block {
        Some(b) => entry.block_number == b,
        None => true, // Skip check
    };
    let is_matching_result_type = match result {
        Some(r) => entry.result_type == r,
        None => true, // Skip check
    };
    is_matching_deployment && is_matching_block && is_matching_result_type
}

#[derive(InputObject)]
struct ResultFilter {
    deployment: Option<String>,
    block_number: Option<u64>,
    result_type: Option<ComparisonResultType>,
}

#[derive(Debug, PartialEq, Eq, Hash, SimpleObject)]
struct CompareRatio {
    deployment: String,
    block_number: u64,
    compare_ratio: String,
}

impl CompareRatio {
    fn new(deployment: String, block_number: u64, compare_ratio: String) -> Self {
        CompareRatio {
            deployment,
            block_number,
            compare_ratio,
        }
    }
}
