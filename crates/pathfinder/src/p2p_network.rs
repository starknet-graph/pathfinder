use std::sync::Arc;

use anyhow::Context;
use p2p::client::{peer_agnostic, peer_aware};
use p2p::libp2p::{identity::Keypair, multiaddr::Multiaddr, PeerId};
use p2p::{HeadRx, HeadTx, Peers};
use pathfinder_common::{BlockHash, BlockNumber, ChainId};
use pathfinder_storage::Storage;
use tokio::sync::RwLock;
use tracing::Instrument;

pub mod client;
mod sync_handlers;

// Silence clippy
pub type P2PNetworkHandle = (
    Arc<RwLock<Peers>>,
    peer_agnostic::Client,
    HeadRx,
    tokio::task::JoinHandle<()>,
);

pub struct P2PContext {
    pub chain_id: ChainId,
    pub storage: Storage,
    pub proxy: bool,
    pub keypair: Keypair,
    pub listen_on: Multiaddr,
    pub bootstrap_addresses: Vec<Multiaddr>,
}

#[tracing::instrument(name = "p2p", skip_all)]
pub async fn start(context: P2PContext) -> anyhow::Result<P2PNetworkHandle> {
    let P2PContext {
        chain_id,
        mut storage,
        proxy,
        keypair,
        listen_on,
        bootstrap_addresses,
    } = context;

    let peer_id = keypair.public().to_peer_id();
    tracing::info!(%peer_id, "🖧 Starting P2P");

    let peers: Arc<RwLock<Peers>> = Arc::new(RwLock::new(Default::default()));
    let (p2p_client, mut p2p_events, p2p_main_loop) =
        p2p::new(keypair, peers.clone(), Default::default());

    let mut main_loop_handle = {
        let span = tracing::info_span!("behaviour");
        tokio::task::spawn(p2p_main_loop.run().instrument(span))
    };

    p2p_client
        .start_listening(listen_on)
        .await
        .context("Starting P2P listener")?;

    for bootstrap_address in bootstrap_addresses {
        let peer_id = bootstrap_address
            .iter()
            .find_map(|p| match p {
                p2p::libp2p::multiaddr::Protocol::P2p(h) => PeerId::from_multihash(h).ok(),
                _ => None,
            })
            .ok_or_else(|| anyhow::anyhow!("Boostrap addresses must inlcude peer ID"))?;
        p2p_client.dial(peer_id, bootstrap_address.clone()).await?;
        p2p_client
            .start_listening(
                bootstrap_address
                    .clone()
                    .with(p2p::libp2p::multiaddr::Protocol::P2pCircuit),
            )
            .await
            .context("Starting relay listener")?;
    }

    let block_propagation_topic = format!("blocks/{}", chain_id.to_hex_str());

    if !proxy {
        p2p_client.subscribe_topic(&block_propagation_topic).await?;
        tracing::info!(topic=%block_propagation_topic, "Subscribed to");
    }

    let capabilities = ["core/block-propagate/1", "core/blocks-sync/1"];
    for capability in capabilities {
        p2p_client.provide_capability(capability).await?
    }

    let (mut tx, rx) = tokio::sync::watch::channel(None);

    let join_handle = {
        let mut p2p_client = p2p_client.clone();
        tokio::task::spawn(
            async move {
                loop {
                    tokio::select! {
                        _ = &mut main_loop_handle => {
                            tracing::error!("p2p task ended unexpectedly");
                            break;
                        }
                        Some(event) = p2p_events.recv() => {
                            match handle_p2p_event(event, &mut storage, &mut p2p_client, &mut tx).await {
                                Ok(()) => {},
                                Err(e) => { tracing::error!("Failed to handle P2P event: {}", e) },
                            }
                        }
                    }
                }
            }
            .in_current_span(),
        )
    };

    Ok((
        peers.clone(),
        peer_agnostic::Client::new(p2p_client, block_propagation_topic, peers),
        rx,
        join_handle,
    ))
}

async fn handle_p2p_event(
    event: p2p::Event,
    storage: &mut Storage,
    p2p_client: &mut peer_aware::Client,
    tx: &mut HeadTx,
) -> anyhow::Result<()> {
    match event {
        _ => {
            todo!("use v1");
        }
        p2p::Event::BlockPropagation { from, new_block } => {
            tracing::info!(%from, ?new_block, "Block Propagation");
            use p2p_proto_v1::block::{BlockHeadersResponse, BlockHeadersResponsePart, NewBlock};

            if let NewBlock::Header(BlockHeadersResponse { mut parts }) = new_block {
                if let Some(BlockHeadersResponsePart::Header(header)) = parts.pop() {
                    tx.send_if_modified(|head| {
                        let current_height = head.unwrap_or_default().0.get();

                        if header.number > current_height {
                            *head = Some((
                                BlockNumber::new_or_panic(header.number),
                                BlockHash(header.block_hash.0),
                            ));
                            true
                        } else {
                            false
                        }
                    });
                }
            }
        }
        p2p::Event::SyncPeerConnected { .. } | p2p::Event::Test(_) => { /* Ignore me */ }
    }

    Ok(())
}
