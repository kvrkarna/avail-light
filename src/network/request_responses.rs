// Copyright 2019-2020 Parity Technologies (UK) Ltd.
// This file is part of Substrate.

// Substrate is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.

// Substrate is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.

// You should have received a copy of the GNU General Public License
// along with Substrate.  If not, see <http://www.gnu.org/licenses/>.

//! Collection of request-response protocols.
//!
//! The [`RequestResponses`] struct defined in this module provides support for zero or more
//! so-called "request-response" protocols.
//!
//! A request-response protocol works in the following way:
//!
//! - For every emitted request, a new substream is open and the protocol is negotiated. If the
//! remote supports the protocol, the size of the request is sent as a LEB128 number, followed
//! with the request itself. The remote then sends the size of the response as a LEB128 number,
//! followed with the response.
//!
//! - Requests have a certain time limit before they time out. This time includes the time it
//! takes to send/receive the request and response.
//!
//! - If provided, a ["requests processing"](RequestResponseConfig::requests_processing) channel
//! is used to handle incoming requests.
//!

use futures::{
    channel::{mpsc, oneshot},
    prelude::*,
};
use libp2p::{
    core::{
        connection::{ConnectionId, ListenerId},
        ConnectedPoint, Multiaddr, PeerId,
    },
    request_response::{
        ProtocolSupport, RequestResponse, RequestResponseCodec, RequestResponseConfig,
        RequestResponseEvent, RequestResponseMessage, ResponseChannel,
    },
    swarm::{
        protocols_handler::multi::MultiHandler, NetworkBehaviour, NetworkBehaviourAction,
        PollParameters, ProtocolsHandler,
    },
};
use std::{
    borrow::Cow,
    collections::{hash_map::Entry, HashMap},
    io, iter,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

pub use libp2p::request_response::{InboundFailure, OutboundFailure, RequestId};

/// Configuration for a single request-response protocol.
#[derive(Debug, Clone)]
pub struct ProtocolConfig {
    /// Name of the protocol on the wire. Should be something like `/foo/bar`.
    pub name: Cow<'static, str>,

    /// Maximum allowed size, in bytes, of a request.
    ///
    /// Any request larger than this value will be declined as a way to avoid allocating too
    /// much memory for the it.
    pub max_request_size: usize,

    /// Maximum allowed size, in bytes, of a response.
    ///
    /// Any response larger than this value will be declined as a way to avoid allocating too
    /// much memory for the it.
    pub max_response_size: usize,

    /// Duration after which emitted requests are considered timed out.
    ///
    /// If you expect the response to come back quickly, you should set this to a smaller duration.
    pub request_timeout: Duration,

    /// Channel on which the networking service will send incoming requests.
    ///
    /// Every time a peer sends a request to the local node using this protocol, the networking
    /// service will push an element on this channel. The receiving side of this channel then has
    /// to pull this element, process the request, and send back the response to send back to the
    /// peer.
    ///
    /// The size of the channel has to be carefully chosen. If the channel is full, the networking
    /// service will discard the incoming request send back an error to the peer. Consequently,
    /// the channel being full is an indicator that the node is overloaded.
    ///
    /// You can typically set the size of the channel to `T / d`, where `T` is the
    /// `request_timeout` and `d` is the expected average duration of CPU and I/O it takes to
    /// build a response.
    ///
    /// Can be `None` if the local node does not support answering incoming requests.
    /// If this is `None`, then the local node will not advertise support for this protocol towards
    /// other peers. If this is `Some` but the channel is closed, then the local node will
    /// advertise support for this protocol, but any incoming request will lead to an error being
    /// sent back.
    pub requests_processing: Option<mpsc::Sender<IncomingRequest>>,
}

/// A single request received by a peer on a request-response protocol.
#[derive(Debug)]
pub struct IncomingRequest {
    /// Who sent the request.
    pub origin: PeerId,

    /// Request sent by the remote. Will always be smaller than
    /// [`RequestResponseConfig::max_response_size`].
    pub request_bytes: Vec<u8>,

    /// Channel to send back the response to.
    pub answer: oneshot::Sender<Vec<u8>>,
}

/// Event generated by the [`RequestResponsesBehaviour`].
#[derive(Debug)]
pub enum Event {
    /// A remote sent a request and either we have successfully answered it or an error happened.
    ///
    /// This event is generated for statistics purposes.
    InboundRequest {
        /// Peer which has emitted the request.
        peer: PeerId,
        /// Name of the protocol in question.
        protocol: Cow<'static, str>,
        /// If `Ok`, contains the time elapsed between when we received the request and when we
        /// sent back the response. If `Err`, the error that happened.
        outcome: Result<Duration, InboundError>,
    },

    /// A request initiated using [`RequestResponsesBehaviour::send_request`] has succeeded or
    /// failed.
    OutboundFinished {
        /// Request that has succeeded.
        request_id: RequestId,
        /// Response sent by the remote or reason for failure.
        outcome: Result<Vec<u8>, OutboundFailure>,
    },
}

/// Implementation of `NetworkBehaviour` that provides support for request-response protocols.
pub struct RequestResponsesBehaviour {
    /// The multiple sub-protocols, by name.
    /// Contains the underlying libp2p `RequestResponse` behaviour, plus an optional
    /// "response builder" used to build responses to incoming requests.
    protocols: HashMap<
        Cow<'static, str>,
        (
            RequestResponse<GenericCodec>,
            Option<mpsc::Sender<IncomingRequest>>,
        ),
    >,

    /// Whenever an incoming request arrives, a `Future` is added to this list and will yield the
    /// response to send back to the remote.
    pending_responses:
        stream::FuturesUnordered<Pin<Box<dyn Future<Output = RequestProcessingOutcome> + Send>>>,
}

/// Generated by the response builder and waiting to be processed.
enum RequestProcessingOutcome {
    PendingResponse {
        protocol: Cow<'static, str>,
        inner_channel: ResponseChannel<Vec<u8>>,
        response: Vec<u8>,
    },
    Busy {
        peer: PeerId,
        protocol: Cow<'static, str>,
    },
}

impl RequestResponsesBehaviour {
    /// Creates a new behaviour. Must be passed a list of supported protocols. Returns an error if
    /// the same protocol is passed twice.
    pub fn new(list: impl Iterator<Item = ProtocolConfig>) -> Result<Self, RegisterError> {
        let mut protocols = HashMap::new();
        for protocol in list {
            let mut cfg = RequestResponseConfig::default();
            cfg.set_connection_keep_alive(Duration::from_secs(10));
            cfg.set_request_timeout(protocol.request_timeout);

            let protocol_support = if protocol.requests_processing.is_some() {
                ProtocolSupport::Full
            } else {
                ProtocolSupport::Outbound
            };

            let rq_rp = RequestResponse::new(
                GenericCodec {
                    max_request_size: protocol.max_request_size,
                    max_response_size: protocol.max_response_size,
                },
                iter::once((protocol.name.as_bytes().to_vec(), protocol_support)),
                cfg,
            );

            match protocols.entry(protocol.name) {
                Entry::Vacant(e) => e.insert((rq_rp, protocol.requests_processing)),
                Entry::Occupied(e) => {
                    return Err(RegisterError::DuplicateProtocol(e.key().clone()))
                }
            };
        }

        Ok(Self {
            protocols,
            pending_responses: stream::FuturesUnordered::new(),
        })
    }

    /// Initiates sending a request.
    ///
    /// An error is returned if we are not connected to the target peer of if the protocol doesn't
    /// match one that has been registered.
    pub fn send_request(
        &mut self,
        target: &PeerId,
        protocol: &str,
        request: Vec<u8>,
    ) -> Result<RequestId, SendRequestError> {
        if let Some((protocol, _)) = self.protocols.get_mut(protocol) {
            if protocol.is_connected(target) {
                Ok(protocol.send_request(target, request))
            } else {
                Err(SendRequestError::NotConnected)
            }
        } else {
            Err(SendRequestError::UnknownProtocol)
        }
    }
}

impl NetworkBehaviour for RequestResponsesBehaviour {
    type ProtocolsHandler =
        MultiHandler<String, <RequestResponse<GenericCodec> as NetworkBehaviour>::ProtocolsHandler>;
    type OutEvent = Event;

    fn new_handler(&mut self) -> Self::ProtocolsHandler {
        let iter = self
            .protocols
            .iter_mut()
            .map(|(p, (r, _))| (p.to_string(), NetworkBehaviour::new_handler(r)));

        MultiHandler::try_from_iter(iter).expect(
            "Protocols are in a HashMap and there can be at most one handler per \
						  protocol name, which is the only possible error; qed",
        )
    }

    fn addresses_of_peer(&mut self, _: &PeerId) -> Vec<Multiaddr> {
        Vec::new()
    }

    fn inject_connection_established(
        &mut self,
        peer_id: &PeerId,
        conn: &ConnectionId,
        endpoint: &ConnectedPoint,
    ) {
        for (p, _) in self.protocols.values_mut() {
            NetworkBehaviour::inject_connection_established(p, peer_id, conn, endpoint)
        }
    }

    fn inject_connected(&mut self, peer_id: &PeerId) {
        for (p, _) in self.protocols.values_mut() {
            NetworkBehaviour::inject_connected(p, peer_id)
        }
    }

    fn inject_connection_closed(
        &mut self,
        peer_id: &PeerId,
        conn: &ConnectionId,
        endpoint: &ConnectedPoint,
    ) {
        for (p, _) in self.protocols.values_mut() {
            NetworkBehaviour::inject_connection_closed(p, peer_id, conn, endpoint)
        }
    }

    fn inject_disconnected(&mut self, peer_id: &PeerId) {
        for (p, _) in self.protocols.values_mut() {
            NetworkBehaviour::inject_disconnected(p, peer_id)
        }
    }

    fn inject_addr_reach_failure(
        &mut self,
        peer_id: Option<&PeerId>,
        addr: &Multiaddr,
        error: &dyn std::error::Error,
    ) {
        for (p, _) in self.protocols.values_mut() {
            NetworkBehaviour::inject_addr_reach_failure(p, peer_id, addr, error)
        }
    }

    fn inject_event(
        &mut self,
        peer_id: PeerId,
        connection: ConnectionId,
        (p_name, event): <Self::ProtocolsHandler as ProtocolsHandler>::OutEvent,
    ) {
        if let Some((proto, _)) = self.protocols.get_mut(&*p_name) {
            return proto.inject_event(peer_id, connection, event);
        }

        log::warn!(target: "sub-libp2p",
			"inject_node_event: no request-response instance registered for protocol {:?}",
			p_name)
    }

    fn inject_new_external_addr(&mut self, addr: &Multiaddr) {
        for (p, _) in self.protocols.values_mut() {
            NetworkBehaviour::inject_new_external_addr(p, addr)
        }
    }

    fn inject_expired_listen_addr(&mut self, addr: &Multiaddr) {
        for (p, _) in self.protocols.values_mut() {
            NetworkBehaviour::inject_expired_listen_addr(p, addr)
        }
    }

    fn inject_dial_failure(&mut self, peer_id: &PeerId) {
        for (p, _) in self.protocols.values_mut() {
            NetworkBehaviour::inject_dial_failure(p, peer_id)
        }
    }

    fn inject_new_listen_addr(&mut self, addr: &Multiaddr) {
        for (p, _) in self.protocols.values_mut() {
            NetworkBehaviour::inject_new_listen_addr(p, addr)
        }
    }

    fn inject_listener_error(&mut self, id: ListenerId, err: &(dyn std::error::Error + 'static)) {
        for (p, _) in self.protocols.values_mut() {
            NetworkBehaviour::inject_listener_error(p, id, err)
        }
    }

    fn inject_listener_closed(&mut self, id: ListenerId, reason: Result<(), &io::Error>) {
        for (p, _) in self.protocols.values_mut() {
            NetworkBehaviour::inject_listener_closed(p, id, reason)
        }
    }

    fn poll(
        &mut self,
        cx: &mut Context,
        params: &mut impl PollParameters,
    ) -> Poll<
        NetworkBehaviourAction<
            <Self::ProtocolsHandler as ProtocolsHandler>::InEvent,
            Self::OutEvent,
        >,
    > {
        // Poll to see if any response is ready to be sent back.
        // We need to check `is_empty` first, otherwise polling would return `None`.
        if !self.pending_responses.is_empty() {
            while let Poll::Ready(Some(outcome)) = self.pending_responses.poll_next_unpin(cx) {
                match outcome {
                    RequestProcessingOutcome::PendingResponse {
                        protocol,
                        inner_channel,
                        response,
                    } => {
                        if let Some((protocol, _)) = self.protocols.get_mut(&*protocol) {
                            protocol.send_response(inner_channel, response);
                        }
                    }
                    RequestProcessingOutcome::Busy { peer, protocol } => {
                        let out = Event::InboundRequest {
                            peer,
                            protocol,
                            outcome: Err(InboundError::Busy),
                        };
                        return Poll::Ready(NetworkBehaviourAction::GenerateEvent(out));
                    }
                }
            }
        }

        // Poll request-responses protocols.
        for (protocol, (behaviour, resp_builder)) in &mut self.protocols {
            while let Poll::Ready(ev) = behaviour.poll(cx, params) {
                let ev = match ev {
                    // Main events we are interested in.
                    NetworkBehaviourAction::GenerateEvent(ev) => ev,

                    // Other events generated by the underlying behaviour are transparently
                    // passed through.
                    NetworkBehaviourAction::DialAddress { address } => {
                        return Poll::Ready(NetworkBehaviourAction::DialAddress { address })
                    }
                    NetworkBehaviourAction::DialPeer { peer_id, condition } => {
                        return Poll::Ready(NetworkBehaviourAction::DialPeer { peer_id, condition })
                    }
                    NetworkBehaviourAction::NotifyHandler {
                        peer_id,
                        handler,
                        event,
                    } => {
                        return Poll::Ready(NetworkBehaviourAction::NotifyHandler {
                            peer_id,
                            handler,
                            event: ((*protocol).to_string(), event),
                        })
                    }
                    NetworkBehaviourAction::ReportObservedAddr { address } => {
                        return Poll::Ready(NetworkBehaviourAction::ReportObservedAddr { address })
                    }
                };

                match ev {
                    // Received a request from a remote.
                    RequestResponseEvent::Message {
                        peer,
                        message: RequestResponseMessage::Request { request, channel },
                    } => {
                        let (tx, rx) = oneshot::channel();

                        // Submit the request to the "response builder" passed by the user at
                        // initialization.
                        if let Some(resp_builder) = resp_builder {
                            // If the response builder is too busy, silently drop `tx`.
                            // This will be reported as a `Busy` error.
                            let _ = resp_builder.try_send(IncomingRequest {
                                origin: peer.clone(),
                                request_bytes: request,
                                answer: tx,
                            });
                        }

                        let protocol = protocol.clone();
                        self.pending_responses.push(Box::pin(async move {
                            // The `tx` created above can be dropped if we are not capable of
                            // processing this request, which is reflected as a "Busy" error.
                            if let Ok(response) = rx.await {
                                RequestProcessingOutcome::PendingResponse {
                                    protocol,
                                    inner_channel: channel,
                                    response,
                                }
                            } else {
                                RequestProcessingOutcome::Busy { peer, protocol }
                            }
                        }));

                        // This `continue` makres sure that `pending_responses` gets polled
                        // after we have added the new element.
                        continue;
                    }

                    // Received a response from a remote to one of our requests.
                    RequestResponseEvent::Message {
                        message:
                            RequestResponseMessage::Response {
                                request_id,
                                response,
                            },
                        ..
                    } => {
                        let out = Event::OutboundFinished {
                            request_id,
                            outcome: Ok(response),
                        };
                        return Poll::Ready(NetworkBehaviourAction::GenerateEvent(out));
                    }

                    // One of our requests has failed.
                    RequestResponseEvent::OutboundFailure {
                        request_id, error, ..
                    } => {
                        let out = Event::OutboundFinished {
                            request_id,
                            outcome: Err(error),
                        };
                        return Poll::Ready(NetworkBehaviourAction::GenerateEvent(out));
                    }

                    // Remote has tried to send a request but failed.
                    RequestResponseEvent::InboundFailure { peer, error } => {
                        let out = Event::InboundRequest {
                            peer,
                            protocol: protocol.clone(),
                            outcome: Err(InboundError::Network(error)),
                        };
                        return Poll::Ready(NetworkBehaviourAction::GenerateEvent(out));
                    }
                };
            }
        }

        Poll::Pending
    }
}

/// Error when registering a protocol.
#[derive(Debug, derive_more::Display, derive_more::Error)]
pub enum RegisterError {
    /// A protocol has been specified multiple times.
    DuplicateProtocol(#[error(ignore)] Cow<'static, str>),
}

/// Error when sending a request.
#[derive(Debug, derive_more::Display, derive_more::Error)]
pub enum SendRequestError {
    /// We are not currently connected to the requested peer.
    NotConnected,
    /// Given protocol hasn't been registered.
    UnknownProtocol,
}

/// Error when processing a request sent by a remote.
#[derive(Debug, derive_more::Display, derive_more::Error)]
pub enum InboundError {
    /// Internal response builder is too busy to process this request.
    Busy,
    /// Problem on the network.
    #[display(fmt = "Problem on the network")]
    Network(#[error(ignore)] InboundFailure),
}

/// Implements the libp2p [`RequestResponseCodec`] trait. Defines how streams of bytes are turned
/// into requests and responses and vice-versa.
#[derive(Debug, Clone)]
#[doc(hidden)] // Needs to be public in order to satisfy the Rust compiler.
pub struct GenericCodec {
    max_request_size: usize,
    max_response_size: usize,
}

#[async_trait::async_trait]
impl RequestResponseCodec for GenericCodec {
    type Protocol = Vec<u8>;
    type Request = Vec<u8>;
    type Response = Vec<u8>;

    async fn read_request<T>(
        &mut self,
        _: &Self::Protocol,
        mut io: &mut T,
    ) -> io::Result<Self::Request>
    where
        T: AsyncRead + Unpin + Send,
    {
        // Read the length.
        let length = unsigned_varint::aio::read_usize(&mut io)
            .await
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        if length > self.max_request_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Request size exceeds limit: {} > {}",
                    length, self.max_request_size
                ),
            ));
        }

        // Read the payload.
        let mut buffer = vec![0; length];
        io.read_exact(&mut buffer).await?;
        Ok(buffer)
    }

    async fn read_response<T>(
        &mut self,
        _: &Self::Protocol,
        mut io: &mut T,
    ) -> io::Result<Self::Response>
    where
        T: AsyncRead + Unpin + Send,
    {
        // Read the length.
        let length = unsigned_varint::aio::read_usize(&mut io)
            .await
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidInput, err))?;
        if length > self.max_response_size {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!(
                    "Response size exceeds limit: {} > {}",
                    length, self.max_response_size
                ),
            ));
        }

        // Read the payload.
        let mut buffer = vec![0; length];
        io.read_exact(&mut buffer).await?;
        Ok(buffer)
    }

    async fn write_request<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        req: Self::Request,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        // TODO: check the length?
        // Write the length.
        {
            let mut buffer = unsigned_varint::encode::usize_buffer();
            io.write_all(unsigned_varint::encode::usize(req.len(), &mut buffer))
                .await?;
        }

        // Write the payload.
        io.write_all(&req).await?;

        io.close().await?;
        Ok(())
    }

    async fn write_response<T>(
        &mut self,
        _: &Self::Protocol,
        io: &mut T,
        res: Self::Response,
    ) -> io::Result<()>
    where
        T: AsyncWrite + Unpin + Send,
    {
        // TODO: check the length?
        // Write the length.
        {
            let mut buffer = unsigned_varint::encode::usize_buffer();
            io.write_all(unsigned_varint::encode::usize(res.len(), &mut buffer))
                .await?;
        }

        // Write the payload.
        io.write_all(&res).await?;

        io.close().await?;
        Ok(())
    }
}
