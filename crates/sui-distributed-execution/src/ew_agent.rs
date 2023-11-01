use std::sync::Arc;

use super::agents::*;
use crate::{dash_store::DashMemoryBackedStore, exec_worker, types::*};
use async_trait::async_trait;
use sui_config::{Config, NodeConfig};
use sui_node::metrics;
use sui_types::metrics::LimitsMetrics;
use tokio::sync::mpsc;

pub struct EWAgent {
    id: UniqueId,
    in_channel: mpsc::Receiver<NetworkMessage>,
    out_channel: mpsc::Sender<NetworkMessage>,
    attrs: GlobalConfig,
}

#[async_trait]
impl Agent<SailfishMessage> for EWAgent {
    fn new(
        id: UniqueId,
        in_channel: mpsc::Receiver<NetworkMessage>,
        out_channel: mpsc::Sender<NetworkMessage>,
        attrs: GlobalConfig,
    ) -> Self {
        EWAgent {
            id,
            in_channel,
            out_channel,
            attrs,
        }
    }

    async fn run(&mut self) {
        println!("Starting EW agent {}", self.id);
        // extract list of all EWs
        let mut ew_ids: Vec<UniqueId> = Vec::new();
        let mut sw_id: UniqueId = 0;
        for (id, entry) in &self.attrs {
            if entry.kind == "EW" {
                ew_ids.push(*id);
            } else {
                sw_id = *id;
            }
        }
        // sort ew_ids
        ew_ids.sort();

        // extract my attrs from the global config
        let my_attrs = &self.attrs.get(&self.id).unwrap().attrs;
        let config_path = my_attrs.get("config").unwrap();
        let metrics_address = my_attrs.get("metrics-address").unwrap().parse().unwrap();
        let config = NodeConfig::load(config_path).unwrap();
        let registry_service = { metrics::start_prometheus_server(metrics_address) };
        let prometheus_registry = registry_service.default_registry();
        let metrics = Arc::new(LimitsMetrics::new(&prometheus_registry));
        let genesis = Arc::new(config.genesis().expect("Could not load genesis"));
        let store = DashMemoryBackedStore::new();

        let mode = {
            if my_attrs["mode"] == "channel" {
                ExecutionMode::Channel
            } else {
                ExecutionMode::Database
            }
        };

        let tx_count = {
            if my_attrs["mode"] == "channel" {
                my_attrs.get("tx_count").unwrap().parse().unwrap()
            } else {
                0
            }
        };

        let mut ew_state = exec_worker::ExecutionWorkerState::new(store, genesis.clone(), mode);
        if my_attrs["mode"] == "database" {
            ew_state.init_store(genesis);
        }

        // Run Sequence Worker asynchronously
        ew_state
            .run(
                metrics,
                tx_count,
                &mut self.in_channel,
                &self.out_channel,
                ew_ids,
                sw_id,
                self.id,
            )
            .await;
    }
}
