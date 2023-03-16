use crate::setup::constants::{MOCK_SUBGRAPH_GOERLI_2, MOCK_SUBGRAPH_MAINNET};
use crate::setup::test_radio::run_test_radio;
use crate::utils::RadioTestConfig;

use poi_radio::{LocalAttestationsMap, MessagesArc, RemoteAttestationsMap};

use tracing::{debug, info};

fn post_comparison_handler(_messages: MessagesArc, _block: u64, _subgraph: &str, _prev_len: usize) {
}

fn test_attestation_handler(
    _block: u64,
    _remote: &RemoteAttestationsMap,
    _local: &LocalAttestationsMap,
) {
}

fn success_handler(messages: MessagesArc) {
    let messages = messages.lock().unwrap();

    // Maybe pass in dynamic count here too
    if messages.len() >= 5 {
        debug!("messages {:?}", messages);

        info!("5 or more valid messages received! Checking content topics");
        assert!(
            messages
                .iter()
                .all(|m| m.identifier == MOCK_SUBGRAPH_MAINNET),
            "Message found with identifier other than {}",
            MOCK_SUBGRAPH_MAINNET
        );

        info!(
            "{}",
            "correct_filtering_different_topics test is successful ✅"
        );
        std::process::exit(0);
    }
}

#[tokio::main]
pub async fn run_correct_filtering_different_topics() {
    let subgraphs = vec![
        MOCK_SUBGRAPH_MAINNET.to_string(),
        MOCK_SUBGRAPH_GOERLI_2.to_string(),
    ];
    let mut config = RadioTestConfig::new(false, true);
    config.subgraphs = Some(subgraphs);
    run_test_radio(
        &config,
        success_handler,
        test_attestation_handler,
        post_comparison_handler,
    )
    .await;
}
