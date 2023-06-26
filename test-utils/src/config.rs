use std::env;

use graphcast_sdk::graphcast_agent::message_typing::IdentityValidation;
use poi_radio::config::{Config, CoverageLevel};

pub fn test_config(
    graph_node_endpoint: String,
    registry_subgraph: String,
    network_subgraph: String,
) -> Config {
    let id = env::var("TEST_RUN_ID").unwrap_or_else(|_| panic!("TEST_RUN_ID not set"));
    let radio_name = format!("poi-radio-test-{}", id);

    Config {
        graph_node_endpoint,
        indexer_address: String::from("0x7e6528e4ce3055e829a32b5dc4450072bac28bc6"),
        private_key: Some(
            "ccaea3e3aca412cb3920dbecd77bc725dfe9a5e16f940f19912d9c9dbee01e8f".to_string(),
        ),
        mnemonic: None,
        registry_subgraph,
        network_subgraph,
        graphcast_network: "testnet".to_string(),
        topics: vec![
            "QmpRkaVUwUQAwPwWgdQHYvw53A5gh3CP3giWnWQZdA2BTE".to_string(),
            "QmtYT8NhPd6msi1btMc3bXgrfhjkJoC4ChcM5tG6fyLjHE".to_string(),
        ],
        coverage: CoverageLevel::OnChain,
        collect_message_duration: 60,
        waku_host: None,
        waku_port: None,
        waku_node_key: None,
        waku_addr: None,
        boot_node_addresses: vec![],
        waku_log_level: None,
        log_level: "off,hyper=off,graphcast_sdk=trace,poi_radio=trace,poi-radio-e2e-tests=trace"
            .to_string(),
        slack_token: None,
        slack_channel: None,
        discord_webhook: None,
        metrics_host: String::new(),
        metrics_port: None,
        server_host: String::new(),
        server_port: None,
        persistence_file_path: Some("./test-runner/state.json".to_string()),
        log_format: "pretty".to_string(),
        radio_name,
        telegram_chat_id: None,
        telegram_token: None,
        discv5_enrs: None,
        discv5_port: None,
        filter_protocol: None,
        id_validation: Some(IdentityValidation::ValidAddress),
    }
}
