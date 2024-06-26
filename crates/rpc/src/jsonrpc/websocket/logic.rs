//! See [the parent module documentation](super)

use std::collections::HashMap;
use std::marker::PhantomData;
use std::num::NonZeroUsize;
use std::ops::ControlFlow;
use std::sync::Arc;

use axum::extract::ws::{Message, WebSocket};
use axum::extract::{State, WebSocketUpgrade};
use axum::response::IntoResponse;
use futures::sink::Buffer;
use futures::stream::{SplitSink, SplitStream};
use futures::{SinkExt, StreamExt};
use serde::Serialize;
use serde_json::Value;
use starknet_gateway_types::reply::Block;
use tokio::sync::{broadcast, mpsc};
use tracing::error;

use crate::jsonrpc::request::RawParams;
use crate::jsonrpc::router::RpcRequestError;
use crate::jsonrpc::websocket::data::{
    EventFilterParams,
    Kind,
    ResponseEvent,
    SubscriptionId,
    SubscriptionItem,
};
use crate::jsonrpc::{RequestId, RpcRequest, RpcRouter};
use crate::method::get_events::types::EmittedEvent;
use crate::BlockHeader;

const SUBSCRIBE_METHOD: &str = "pathfinder_subscribe";
const UNSUBSCRIBE_METHOD: &str = "pathfinder_unsubscribe";
const NEW_HEADS_TOPIC: &str = "newHeads";
const EVENTS_TOPIC: &str = "events";

#[derive(Clone)]
pub struct WebsocketContext {
    socket_buffer_capacity: NonZeroUsize,
    pub broadcasters: TopicBroadcasters,
}

impl WebsocketContext {
    pub fn new(socket_buffer_capacity: NonZeroUsize, topic_sender_capacity: NonZeroUsize) -> Self {
        let senders = TopicBroadcasters::with_capacity(topic_sender_capacity);

        Self {
            socket_buffer_capacity,
            broadcasters: senders,
        }
    }
}

impl Default for WebsocketContext {
    fn default() -> Self {
        Self {
            socket_buffer_capacity: NonZeroUsize::new(100)
                .expect("Invalid socket buffer capacity default value"),
            broadcasters: TopicBroadcasters::default(),
        }
    }
}

pub async fn websocket_handler(
    ws: WebSocketUpgrade,
    State(router): State<RpcRouter>,
) -> impl IntoResponse {
    let mut upgrade_response = ws
        .max_message_size(crate::REQUEST_MAX_SIZE)
        .on_failed_upgrade(|error| tracing::debug!(%error, "Websocket upgrade failed"))
        .on_upgrade(|socket| handle_socket(socket, router));

    static APPLICATION_JSON: http::HeaderValue = http::HeaderValue::from_static("application/json");
    upgrade_response
        .headers_mut()
        .insert(http::header::CONTENT_TYPE, APPLICATION_JSON.clone());

    upgrade_response
}

async fn handle_socket(socket: WebSocket, router: RpcRouter) {
    let websocket_context = router
        .context
        .websocket
        .as_ref()
        .expect("Websocket handler should not be called with Websocket disabled");
    let (ws_sender, ws_receiver) = socket.split();

    let (response_sender, response_receiver) = mpsc::channel(10);

    tokio::spawn(write(
        ws_sender,
        response_receiver,
        websocket_context.socket_buffer_capacity,
    ));
    tokio::spawn(read(ws_receiver, response_sender, router));
}

async fn write(
    sender: SplitSink<WebSocket, Message>,
    mut response_receiver: mpsc::Receiver<ResponseEvent>,
    buffer_capacity: NonZeroUsize,
) {
    let mut sender = sender.buffer(buffer_capacity.get());
    while let Some(response) = response_receiver.recv().await {
        if let ControlFlow::Break(()) = send_response(&mut sender, &response).await {
            break;
        }
    }
}

async fn send_response(
    sender: &mut Buffer<SplitSink<WebSocket, Message>, Message>,
    response: &ResponseEvent,
) -> ControlFlow<()> {
    let message = match serde_json::to_string(&response) {
        Ok(x) => x,
        Err(e) => {
            tracing::warn!(error=%e, kind=response.kind(), "Encoding websocket message failed");
            return ControlFlow::Break(());
        }
    };

    // `send` implies a systematical flush.
    // We may want to poll the receiver less eagerly, flushing only once the `recv`
    // is `NotReady`, but because we won't get multiple heads coming in a row I
    // fear this would bring noticeable complexity for a negligible improvement
    if let Err(e) = sender.send(Message::Text(message)).await {
        // What could cause this failure? Probably the client closing the connection..
        // And a full buffer.
        tracing::debug!(error=%e, "Sending websocket message failed");
        return ControlFlow::Break(());
    }

    ControlFlow::Continue(())
}

async fn read(
    mut receiver: SplitStream<WebSocket>,
    response_sender: mpsc::Sender<ResponseEvent>,
    router: RpcRouter,
) {
    let websocket_context = router
        .context
        .websocket
        .as_ref()
        .expect("Websocket handler should not be called with Websocket disabled");
    let source = &websocket_context.broadcasters;
    let mut subscription_manager = SubscriptionManager::default();

    loop {
        let request = match receiver.next().await {
            Some(Ok(Message::Text(x))) => x.into_bytes(),
            Some(Ok(Message::Binary(x))) => x,
            Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {
                // Ping and pong messages are handled automatically by axum.
                continue;
            }
            // All of the following indicate client disconnection.
            Some(Err(e)) => {
                tracing::trace!(error=%e, "Client disconnected");
                break;
            }
            Some(Ok(Message::Close(_))) | None => {
                tracing::trace!("Client disconnected");
                break;
            }
        };

        let parsed_request = match serde_json::from_slice::<RpcRequest<'_>>(&request) {
            Ok(request) => request,
            Err(err) => {
                match response_sender.try_send(ResponseEvent::InvalidRequest(err.to_string())) {
                    Ok(_) => continue,
                    Err(e) => {
                        tracing::debug!(reason=%e, "Failed to send invalid request response");
                        break;
                    }
                }
            }
        };

        // Handle request.
        let response = match parsed_request.method.as_ref() {
            SUBSCRIBE_METHOD => subscription_manager.subscribe(
                parsed_request.id,
                parsed_request.params,
                response_sender.clone(),
                source.clone(),
            ),
            UNSUBSCRIBE_METHOD => {
                subscription_manager
                    .unsubscribe(parsed_request.id, parsed_request.params)
                    .await
            }
            _ => match super::super::router::handle_json_rpc_body(&router, &request).await {
                Ok(responses) => ResponseEvent::Responses(responses),
                Err(RpcRequestError::ParseError(e)) => ResponseEvent::InvalidRequest(e),
                Err(RpcRequestError::InvalidRequest(e)) => ResponseEvent::InvalidRequest(e),
            },
        };

        if let Err(e) = response_sender.try_send(response) {
            tracing::debug!(reason=%e, "Failed to send response");
            break;
        }
    }

    // Force some clean up by aborting all still running subscriptions.
    // These would naturally come to a halt as the message queues break,
    // but this will kill them more quickly.
    subscription_manager.abort_all();
}

/// Manages the subscription for a single connection
#[derive(Default)]
struct SubscriptionManager {
    next_id: u32,
    subscriptions: HashMap<u32, tokio::task::JoinHandle<()>>,
}

impl SubscriptionManager {
    async fn unsubscribe(
        &mut self,
        request_id: RequestId,
        request_params: RawParams<'_>,
    ) -> ResponseEvent {
        let subscription_id = match request_params.deserialize::<SubscriptionId>() {
            Ok(x) => x,
            Err(crate::jsonrpc::RpcError::InvalidParams(e)) => {
                return ResponseEvent::InvalidParams(request_id, e)
            }
            Err(_) => {
                return ResponseEvent::InvalidParams(
                    request_id,
                    "Unexpected parsing error".to_owned(),
                )
            }
        };

        let success = match self.subscriptions.remove(&subscription_id.id) {
            Some(handle) => {
                handle.abort();
                if let Some(err) = handle.await.err().filter(|e| !e.is_cancelled()) {
                    error!("Websocket subscription join error: {}", err);
                }
                true
            }
            None => false,
        };

        ResponseEvent::Unsubscribed {
            success,
            request_id,
        }
    }

    fn subscribe(
        &mut self,
        request_id: RequestId,
        request_params: RawParams<'_>,
        response_sender: mpsc::Sender<ResponseEvent>,
        websocket_source: TopicBroadcasters,
    ) -> ResponseEvent {
        let kind = match request_params.deserialize::<Kind<'_>>() {
            Ok(x) => x,
            Err(crate::jsonrpc::RpcError::InvalidParams(e)) => {
                return ResponseEvent::InvalidParams(request_id, e)
            }
            Err(_) => {
                return ResponseEvent::InvalidParams(
                    request_id,
                    "Unexpected parsing error".to_owned(),
                )
            }
        };

        let subscription_id = self.next_id;
        self.next_id += 1;
        let handle = match kind.kind.as_ref() {
            NEW_HEADS_TOPIC => {
                let receiver = websocket_source.new_head.subscribe();
                tokio::spawn(header_subscription(
                    response_sender,
                    receiver,
                    subscription_id,
                ))
            }
            EVENTS_TOPIC => {
                let filter = match request_params.deserialize::<EventFilterParams>() {
                    Ok(x) => x,
                    Err(crate::jsonrpc::RpcError::InvalidParams(e)) => {
                        return ResponseEvent::InvalidParams(request_id, e)
                    }
                    Err(_) => {
                        return ResponseEvent::InvalidParams(
                            request_id,
                            "Unexpected parsing error".to_owned(),
                        )
                    }
                };
                let receiver = websocket_source.blocks.subscribe();
                tokio::spawn(event_subscription(
                    response_sender,
                    receiver,
                    subscription_id,
                    filter,
                ))
            }
            _ => {
                return ResponseEvent::InvalidParams(
                    request_id,
                    "Unknown subscription type".to_owned(),
                )
            }
        };

        self.subscriptions.insert(subscription_id, handle);

        ResponseEvent::Subscribed {
            subscription_id,
            request_id,
        }
    }

    fn abort_all(self) {
        for (_, handle) in self.subscriptions {
            handle.abort();
        }
    }
}

async fn header_subscription(
    msg_sender: mpsc::Sender<ResponseEvent>,
    mut headers: broadcast::Receiver<Arc<Value>>,
    subscription_id: u32,
) {
    use broadcast::error::RecvError;
    loop {
        let response = match headers.recv().await {
            Ok(header) => ResponseEvent::Header(SubscriptionItem {
                subscription_id,
                item: header,
            }),
            Err(RecvError::Closed) => break,
            Err(RecvError::Lagged(amount)) => {
                tracing::debug!(%subscription_id, %amount, kind="header", "Subscription consumer too slow, closing.");

                // No explicit break here, the loop will be broken by the dropped receiver.
                ResponseEvent::SubscriptionClosed {
                    subscription_id,
                    reason: "Lagging stream, some headers were skipped. Closing subscription."
                        .to_owned(),
                }
            }
        };

        if msg_sender.send(response).await.is_err() {
            break;
        }
    }
}

async fn event_subscription(
    msg_sender: mpsc::Sender<ResponseEvent>,
    mut blocks: broadcast::Receiver<Arc<Block>>,
    subscription_id: u32,
    filter: EventFilterParams,
) {
    use broadcast::error::RecvError;
    let key_filter_is_empty = filter.keys.iter().flatten().count() == 0;
    let keys: Vec<std::collections::HashSet<_>> = filter
        .keys
        .iter()
        .map(|keys| keys.iter().collect())
        .collect();
    'outer: loop {
        match blocks.recv().await {
            Ok(block) => {
                for (receipt, events) in block.transaction_receipts.iter() {
                    for event in events {
                        // Check if the event matches the filter.
                        if let Some(address) = filter.address {
                            if event.from_address != address {
                                continue;
                            }
                        }
                        let matches_keys = if key_filter_is_empty {
                            true
                        } else if event.keys.len() < keys.len() {
                            false
                        } else {
                            event
                                .keys
                                .iter()
                                .zip(keys.iter())
                                .all(|(key, filter)| filter.is_empty() || filter.contains(key))
                        };
                        if !matches_keys {
                            continue;
                        }

                        let response = ResponseEvent::Event(SubscriptionItem {
                            subscription_id,
                            item: Arc::new(EmittedEvent {
                                data: event.data.clone(),
                                keys: event.keys.clone(),
                                from_address: event.from_address,
                                block_hash: Some(block.block_hash),
                                block_number: Some(block.block_number),
                                transaction_hash: receipt.transaction_hash,
                            }),
                        });
                        if msg_sender.send(response).await.is_err() {
                            break 'outer;
                        }
                    }
                }
            }
            Err(RecvError::Closed) => break,
            Err(RecvError::Lagged(amount)) => {
                tracing::debug!(%subscription_id, %amount, kind="event", "Subscription consumer too slow, closing.");

                // No explicit break here, the loop will be broken by the dropped receiver.
                let response = ResponseEvent::SubscriptionClosed {
                    subscription_id,
                    reason: "Lagging stream, some events were skipped. Closing subscription."
                        .to_owned(),
                };
                if msg_sender.send(response).await.is_err() {
                    break;
                }
            }
        };
    }
}

/// A Tokio broadcast sender pre-serializing the value once for all subscribers.
/// Relies on `Arc`s to flatten the cloning costs inherent to Tokio broadcast
/// channels.
#[derive(Debug, Clone)]
pub struct JsonBroadcaster<T> {
    sender: broadcast::Sender<Arc<Value>>,
    item_type: PhantomData<T>,
}

impl<T> JsonBroadcaster<T>
where
    T: Serialize,
{
    pub fn send_if_receiving(&self, item: T) -> Result<(), serde_json::Error> {
        if self.sender.receiver_count() > 0 {
            tracing::debug!("Broadcasting");

            // This won't cut all of serialization costs but it's a simple compromise.
            // At least things like string encoding will be performed once only.
            let value = serde_json::to_value(item)?;
            // Tokio broadcast channels clone the items for each subscriber.
            // Embed the value in an `Arc` to flatten this cost.
            let value = Arc::new(value);

            if let Err(err) = self.sender.send(value) {
                tracing::warn!("Broadcasting failed, the buffer might be full: {}", err);
            }
        } else {
            tracing::debug!("No receivers, skipping the broadcast");
        }

        Ok(())
    }

    pub fn subscribe(&self) -> broadcast::Receiver<Arc<Value>> {
        self.sender.subscribe()
    }
}

#[derive(Debug, Clone)]
pub struct TopicBroadcasters {
    pub new_head: JsonBroadcaster<BlockHeader>,
    pub blocks: broadcast::Sender<Arc<Block>>,
}

impl TopicBroadcasters {
    fn with_capacity(capacity: NonZeroUsize) -> TopicBroadcasters {
        TopicBroadcasters {
            new_head: JsonBroadcaster {
                sender: broadcast::channel(capacity.get()).0,
                item_type: PhantomData {},
            },
            blocks: broadcast::channel(capacity.get()).0,
        }
    }
}

impl Default for TopicBroadcasters {
    fn default() -> Self {
        TopicBroadcasters::with_capacity(
            NonZeroUsize::new(100).expect("Invalid default broadcaster capacity"),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::borrow::Cow;
    use std::time::Duration;

    use axum::routing::get;
    use futures::{SinkExt, StreamExt};
    use pathfinder_common::event::Event;
    use pathfinder_common::{
        block_hash,
        event_commitment,
        event_key,
        state_commitment,
        transaction_commitment,
        transaction_hash,
        BlockNumber,
        BlockTimestamp,
        ContractAddress,
        EventData,
        EventKey,
        GasPrice,
        StarknetVersion,
    };
    use pathfinder_crypto::Felt;
    use pretty_assertions_sorted::assert_eq;
    use serde::Serialize;
    use serde_json::value::RawValue;
    use serde_json::{json, Number, Value};
    use starknet_gateway_types::reply::GasPrices;
    use tokio::net::TcpStream;
    use tokio::task::JoinHandle;
    use tokio::time::timeout;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

    use super::*;
    use crate::context::RpcContext;
    use crate::jsonrpc::websocket::data::successful_response;
    use crate::jsonrpc::{RpcError, RpcResponse};

    #[tokio::test]
    async fn params_are_required() {
        let mut client = Client::new().await;

        client
            .send_request(&RpcRequest {
                method: Cow::from(SUBSCRIBE_METHOD),
                params: Default::default(),
                id: RequestId::Null,
            })
            .await;

        client
            .expect_response(&RpcResponse {
                output: Err(RpcError::InvalidParams(
                    "EOF while parsing a value at line 1 column 0".to_owned(),
                )),
                id: RequestId::Null,
            })
            .await;

        client.destroy().await;
    }

    #[tokio::test]
    async fn subscribe_new_heads() {
        let mut client = Client::new().await;

        let req_id = RequestId::Number(37);
        client
            .send_request(&RpcRequest {
                method: Cow::from(SUBSCRIBE_METHOD),
                params: RawParams(Some(
                    &RawValue::from_string(r#"["newHeads"]"#.to_owned()).unwrap(),
                )),
                id: req_id.clone(),
            })
            .await;

        let expected_subscription_id = 0;
        client
            .expect_response(&successful_response(&expected_subscription_id, req_id).unwrap())
            .await;

        // Do this a bunch of times to ensure the test reception timeout is long enough.
        for _i in 0..10 {
            let header = header_sample();
            client
                .head_sender
                .send_if_receiving(header.clone())
                .unwrap();

            client
                .expect_response(&SubscriptionItem {
                    subscription_id: 0,
                    item: header,
                })
                .await;
        }

        let req_id = RequestId::String("req_id".into());
        client
            .send_request(&RpcRequest {
                method: Cow::from(UNSUBSCRIBE_METHOD),
                params: RawParams(Some(&value(&SubscriptionId {
                    id: expected_subscription_id,
                }))),
                id: req_id.clone(),
            })
            .await;
        client
            .expect_response(&successful_response(&true, req_id).unwrap())
            .await;

        // Now make sure we don't receive it. This is why testing the timeout was
        // important.
        client
            .head_sender
            .send_if_receiving(header_sample())
            .unwrap();
        client.expect_no_response().await;

        client.destroy().await;
    }

    #[tokio::test]
    async fn fall_back_to_rpc_method() {
        let mut client = Client::new().await;

        client
            .send_request(&RpcRequest {
                method: Cow::from("pathfinder_test"),
                params: Default::default(),
                id: RequestId::Number(1),
            })
            .await;

        client
            .expect_response(&RpcResponse {
                output: Ok(json!("0x534e5f5345504f4c4941")),
                id: RequestId::Number(1),
            })
            .await;

        client.destroy().await;
    }

    #[tokio::test]
    async fn subscribe_events() {
        let mut client = Client::new().await;
        let block = block_sample();

        let req_id = RequestId::Number(37);
        client
            .send_request(&RpcRequest {
                method: Cow::from(SUBSCRIBE_METHOD),
                params: RawParams(Some(
                    &RawValue::from_string(r#"["events"]"#.to_owned()).unwrap(),
                )),
                id: req_id.clone(),
            })
            .await;

        client
            .expect_response(&successful_response(&0, req_id).unwrap())
            .await;

        client.blocks_sender.send(block.clone().into()).unwrap();

        client
            .expect_response(&SubscriptionItem {
                subscription_id: 0,
                item: EmittedEvent {
                    from_address: ContractAddress::new_or_panic(Felt::from_hex_str("2").unwrap()),
                    data: vec![EventData(Felt::from_hex_str("a").unwrap())],
                    keys: vec![
                        EventKey(Felt::from_hex_str("b").unwrap()),
                        event_key!("0xdeadbeef"),
                    ],
                    block_hash: Some(block_hash!("0x1")),
                    block_number: Some(BlockNumber::new_or_panic(1)),
                    transaction_hash: transaction_hash!("0x1"),
                },
            })
            .await;
        client
            .expect_response(&SubscriptionItem {
                subscription_id: 0,
                item: EmittedEvent {
                    from_address: ContractAddress::new_or_panic(Felt::from_hex_str("3").unwrap()),
                    data: vec![EventData(Felt::from_hex_str("c").unwrap())],
                    keys: vec![
                        EventKey(Felt::from_hex_str("d").unwrap()),
                        event_key!("0xcafebabe"),
                    ],
                    block_hash: Some(block_hash!("0x1")),
                    block_number: Some(BlockNumber::new_or_panic(1)),
                    transaction_hash: transaction_hash!("0x2"),
                },
            })
            .await;
        client
            .expect_response(&SubscriptionItem {
                subscription_id: 0,
                item: EmittedEvent {
                    from_address: ContractAddress::new_or_panic(Felt::from_hex_str("4").unwrap()),
                    data: vec![EventData(Felt::from_hex_str("e").unwrap())],
                    keys: vec![
                        EventKey(Felt::from_hex_str("f").unwrap()),
                        event_key!("0x1234"),
                    ],
                    block_hash: Some(block_hash!("0x1")),
                    block_number: Some(BlockNumber::new_or_panic(1)),
                    transaction_hash: transaction_hash!("0x2"),
                },
            })
            .await;

        client.expect_no_response().await;

        let req_id = RequestId::String("unsub_1".into());
        client
            .send_request(&RpcRequest {
                method: Cow::from(UNSUBSCRIBE_METHOD),
                params: RawParams(Some(&value(&SubscriptionId { id: 0 }))),
                id: req_id.clone(),
            })
            .await;
        client
            .expect_response(&successful_response(&true, req_id).unwrap())
            .await;

        let req_id = RequestId::Number(38);
        client
            .send_request(&RpcRequest {
                method: Cow::from(SUBSCRIBE_METHOD),
                params: RawParams(Some(
                    &RawValue::from_string(
                        r#"{"kind": "events", "keys": [[], ["0xdeadbeef"]]}"#.to_owned(),
                    )
                    .unwrap(),
                )),
                id: req_id.clone(),
            })
            .await;

        client
            .expect_response(&successful_response(&1, req_id).unwrap())
            .await;

        client.blocks_sender.send(block.clone().into()).unwrap();

        client
            .expect_response(&SubscriptionItem {
                subscription_id: 1,
                item: EmittedEvent {
                    from_address: ContractAddress::new_or_panic(Felt::from_hex_str("2").unwrap()),
                    data: vec![EventData(Felt::from_hex_str("a").unwrap())],
                    keys: vec![
                        EventKey(Felt::from_hex_str("b").unwrap()),
                        event_key!("0xdeadbeef"),
                    ],
                    block_hash: Some(block_hash!("0x1")),
                    block_number: Some(BlockNumber::new_or_panic(1)),
                    transaction_hash: transaction_hash!("0x1"),
                },
            })
            .await;

        client.expect_no_response().await;

        let req_id = RequestId::String("unsub_2".into());
        client
            .send_request(&RpcRequest {
                method: Cow::from(UNSUBSCRIBE_METHOD),
                params: RawParams(Some(&value(&SubscriptionId { id: 1 }))),
                id: req_id.clone(),
            })
            .await;
        client
            .expect_response(&successful_response(&true, req_id).unwrap())
            .await;

        let req_id = RequestId::Number(39);
        client
            .send_request(&RpcRequest {
                method: Cow::from(SUBSCRIBE_METHOD),
                params: RawParams(Some(
                    &RawValue::from_string(r#"{"kind": "events", "address": "0x3"}"#.to_owned())
                        .unwrap(),
                )),
                id: req_id.clone(),
            })
            .await;

        client
            .expect_response(&successful_response(&2, req_id).unwrap())
            .await;

        client.blocks_sender.send(block.into()).unwrap();

        client
            .expect_response(&SubscriptionItem {
                subscription_id: 2,
                item: EmittedEvent {
                    from_address: ContractAddress::new_or_panic(Felt::from_hex_str("3").unwrap()),
                    data: vec![EventData(Felt::from_hex_str("c").unwrap())],
                    keys: vec![
                        EventKey(Felt::from_hex_str("d").unwrap()),
                        event_key!("0xcafebabe"),
                    ],
                    block_hash: Some(block_hash!("0x1")),
                    block_number: Some(BlockNumber::new_or_panic(1)),
                    transaction_hash: transaction_hash!("0x2"),
                },
            })
            .await;

        client.expect_no_response().await;

        client.destroy().await;
    }

    // TODO Prevent duplicate subscriptions?
    // This is actually tolerated by Alchemy, you can subscribe multiple times
    // to the same topic and receive duplicated messages as a result.
    // TODO Subscription limit?

    fn value<S>(payload: &S) -> Box<RawValue>
    where
        S: Serialize + ?Sized,
    {
        RawValue::from_string(serde_json::to_string(payload).unwrap()).unwrap()
    }

    fn header_sample() -> BlockHeader {
        BlockHeader(Default::default())
    }

    fn block_sample() -> Block {
        Block {
            block_hash: block_hash!("0x1"),
            block_number: BlockNumber::new_or_panic(1),
            l1_gas_price: GasPrices {
                price_in_wei: GasPrice(0),
                price_in_fri: GasPrice(0),
            },
            l1_data_gas_price: GasPrices {
                price_in_wei: GasPrice(0),
                price_in_fri: GasPrice(0),
            },
            parent_block_hash: block_hash!("0x2"),
            sequencer_address: None,
            state_commitment: state_commitment!("0x3"),
            status: starknet_gateway_types::reply::Status::AcceptedOnL2,
            timestamp: BlockTimestamp::new_or_panic(1),
            transaction_receipts: vec![
                (
                    pathfinder_common::receipt::Receipt {
                        transaction_hash: transaction_hash!("0x1"),
                        ..Default::default()
                    },
                    vec![Event {
                        from_address: ContractAddress::new_or_panic(
                            Felt::from_hex_str("2").unwrap(),
                        ),
                        data: vec![EventData(Felt::from_hex_str("a").unwrap())],
                        keys: vec![
                            EventKey(Felt::from_hex_str("b").unwrap()),
                            event_key!("0xdeadbeef"),
                        ],
                    }],
                ),
                (
                    pathfinder_common::receipt::Receipt {
                        transaction_hash: transaction_hash!("0x2"),
                        ..Default::default()
                    },
                    vec![
                        Event {
                            from_address: ContractAddress::new_or_panic(
                                Felt::from_hex_str("3").unwrap(),
                            ),
                            data: vec![EventData(Felt::from_hex_str("c").unwrap())],
                            keys: vec![
                                EventKey(Felt::from_hex_str("d").unwrap()),
                                event_key!("0xcafebabe"),
                            ],
                        },
                        Event {
                            from_address: ContractAddress::new_or_panic(
                                Felt::from_hex_str("4").unwrap(),
                            ),
                            data: vec![EventData(Felt::from_hex_str("e").unwrap())],
                            keys: vec![
                                EventKey(Felt::from_hex_str("f").unwrap()),
                                event_key!("0x1234"),
                            ],
                        },
                    ],
                ),
            ],
            transactions: vec![],
            starknet_version: StarknetVersion::new(1, 1, 1, 1),
            transaction_commitment: transaction_commitment!("0x4"),
            event_commitment: event_commitment!("0x5"),
            l1_da_mode: starknet_gateway_types::reply::L1DataAvailabilityMode::Blob,
        }
    }

    struct Client {
        sender: SplitSink<WebSocketStream<MaybeTlsStream<TcpStream>>, Message>,
        receiver: SplitStream<WebSocketStream<MaybeTlsStream<TcpStream>>>,
        server_handle: JoinHandle<()>,
        head_sender: JsonBroadcaster<BlockHeader>,
        blocks_sender: broadcast::Sender<Arc<Block>>,
    }

    impl Client {
        async fn new() -> Client {
            let context = RpcContext::for_tests().with_websockets(WebsocketContext::default());
            let router = RpcRouter::builder(crate::RpcVersion::V07)
                .register("pathfinder_test", rpc_test_method)
                .build(context.clone());
            let websocket_context = context.websocket.unwrap_or_default();
            let head_sender = websocket_context.broadcasters.new_head.clone();
            let blocks_sender = websocket_context.broadcasters.blocks.clone();

            let router = axum::Router::new()
                .route("/ws", get(websocket_handler))
                .with_state(router)
                .layer(tower::ServiceBuilder::new());

            let listener = std::net::TcpListener::bind("127.0.0.1:0")
                .expect("Websocket address already in use");
            let addr = listener.local_addr().unwrap();
            let server = axum::Server::from_tcp(listener).unwrap();
            let server_handle =
                tokio::spawn(
                    async move { server.serve(router.into_make_service()).await.unwrap() },
                );

            let ws_addr = "ws://".to_string() + &addr.to_string() + "/ws";
            let ws_stream = match connect_async(ws_addr).await {
                Ok((stream, _response)) => stream,
                Err(e) => {
                    panic!("WebSocket handshake failed with {e}!");
                }
            };

            let (sender, receiver) = ws_stream.split();

            Client {
                head_sender,
                blocks_sender,
                sender,
                receiver,
                server_handle,
            }
        }

        async fn send_request(&mut self, request: &RpcRequest<'_>) {
            let id = match &request.id {
                RequestId::Number(n) => Value::Number(Number::from(*n)),
                RequestId::String(s) => Value::String(s.to_string()),
                RequestId::Null => Value::Null,
                RequestId::Notification => Value::String("notification".to_string()),
            };
            let json = serde_json::to_string(&json!({
                "jsonrpc": "2.0",
                "method": request.method,
                "id": id,
                "params": request.params,
            }))
            .unwrap();
            self.sender.send(Message::Text(json)).await.unwrap();
        }

        async fn expect_response<R>(&mut self, response: &R)
        where
            R: Serialize,
        {
            let message = timeout(Duration::from_millis(100), self.receiver.next())
                .await
                .unwrap()
                .unwrap()
                .unwrap();
            let Message::Text(raw_text) = message else {
                panic!("Unexpected type of message")
            };

            // Deserialize it to a generic value to avoid field ordering issues.
            let received: Value = serde_json::from_str(&raw_text).unwrap();
            let expected = serde_json::to_value(response).unwrap();
            assert_eq!(received, expected);
        }

        async fn expect_no_response(&mut self) {
            let timeout_result = timeout(Duration::from_millis(100), self.receiver.next()).await;

            match timeout_result {
                Ok(Some(_)) => {
                    panic!("Unexpected message received")
                }
                Ok(None) => {
                    panic!("Connection closed unexpectedly")
                }
                Err(_) => {
                    // Expected
                }
            }
        }

        async fn destroy(mut self) {
            self.sender.send(Message::Close(None)).await.unwrap();

            self.server_handle.abort();
            let _ignored = self.server_handle.await;
        }
    }

    pub async fn rpc_test_method(
        context: RpcContext,
    ) -> Result<pathfinder_common::ChainId, RpcError> {
        Ok(context.chain_id)
    }
}
