// Copyright 2021 Protocol Labs.
//
// Permission is hereby granted, free of charge, to any person obtaining a
// copy of this software and associated documentation files (the "Software"),
// to deal in the Software without restriction, including without limitation
// the rights to use, copy, modify, merge, publish, distribute, sublicense,
// and/or sell copies of the Software, and to permit persons to whom the
// Software is furnished to do so, subject to the following conditions:
//
// The above copyright notice and this permission notice shall be included in
// all copies or substantial portions of the Software.
//
// THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
// OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
// FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
// AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
// LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING
// FROM, OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER
// DEALINGS IN THE SOFTWARE.

//! [`NetworkBehaviour`] to act as a circuit relay v2 **client**.

mod handler;
pub mod transport;

use crate::v2::protocol::inbound_stop;
use bytes::Bytes;
use futures::channel::mpsc::{Receiver, Sender};
use futures::channel::oneshot;
use futures::future::{BoxFuture, FutureExt};
use futures::io::{AsyncRead, AsyncWrite};
use futures::ready;
use futures::stream::StreamExt;
use libp2p_core::connection::{ConnectedPoint, ConnectionId};
use libp2p_core::{Multiaddr, PeerId};
use libp2p_swarm::dial_opts::DialOpts;
use libp2p_swarm::{
    DialError, NegotiatedSubstream, NetworkBehaviour, NetworkBehaviourAction, NotifyHandler,
    PollParameters,
};
use std::collections::{HashMap, VecDeque};
use std::io::{Error, IoSlice};
use std::ops::DerefMut;
use std::pin::Pin;
use std::task::{Context, Poll};

/// The events produced by the [`Client`] behaviour.
#[derive(Debug)]
pub enum Event {
    /// An outbound reservation has been accepted.
    ReservationReqAccepted {
        relay_peer_id: PeerId,
        /// Indicates whether the request replaces an existing reservation.
        renewal: bool,
    },
    ReservationReqFailed {
        relay_peer_id: PeerId,
        /// Indicates whether the request replaces an existing reservation.
        renewal: bool,
    },
    OutboundCircuitReqFailed {
        relay_peer_id: PeerId,
    },
    /// An inbound circuit request has been denied.
    InboundCircuitReqDenied {
        src_peer_id: PeerId,
    },
    /// Denying an inbound circuit request failed.
    InboundCircuitReqDenyFailed {
        src_peer_id: PeerId,
        error: std::io::Error,
    },
}

pub struct Client {
    local_peer_id: PeerId,

    from_transport: Receiver<transport::TransportToBehaviourMsg>,
    connected_peers: HashMap<PeerId, Vec<ConnectionId>>,
    rqsts_pending_connection: HashMap<PeerId, Vec<RqstPendingConnection>>,

    /// Queue of actions to return when polled.
    queued_actions: VecDeque<NetworkBehaviourAction<Event, handler::Prototype>>,
}

impl Client {
    pub fn new_transport_and_behaviour(
        local_peer_id: PeerId,
    ) -> (transport::ClientTransport, Self) {
        let (transport, from_transport) = transport::ClientTransport::new();
        let behaviour = Client {
            local_peer_id,
            from_transport,
            connected_peers: Default::default(),
            rqsts_pending_connection: Default::default(),
            queued_actions: Default::default(),
        };
        (transport, behaviour)
    }
}

impl NetworkBehaviour for Client {
    type ProtocolsHandler = handler::Prototype;
    type OutEvent = Event;

    fn new_handler(&mut self) -> Self::ProtocolsHandler {
        handler::Prototype::new(self.local_peer_id)
    }

    fn addresses_of_peer(&mut self, _: &PeerId) -> Vec<Multiaddr> {
        vec![]
    }

    fn inject_connected(&mut self, _peer_id: &PeerId) {}

    fn inject_connection_established(
        &mut self,
        peer_id: &PeerId,
        connection_id: &ConnectionId,
        _: &ConnectedPoint,
        _failed_addresses: Option<&Vec<Multiaddr>>,
    ) {
        self.connected_peers
            .entry(*peer_id)
            .or_default()
            .push(*connection_id);

        for rqst in self
            .rqsts_pending_connection
            .remove(peer_id)
            .map(|rqsts| rqsts.into_iter())
            .into_iter()
            .flatten()
        {
            match rqst {
                RqstPendingConnection::Reservation { to_listener, .. } => {
                    self.queued_actions
                        .push_back(NetworkBehaviourAction::NotifyHandler {
                            peer_id: *peer_id,
                            handler: NotifyHandler::One(*connection_id),
                            event: handler::In::Reserve { to_listener },
                        });
                }
                RqstPendingConnection::Circuit {
                    send_back,
                    dst_peer_id,
                    ..
                } => {
                    self.queued_actions
                        .push_back(NetworkBehaviourAction::NotifyHandler {
                            peer_id: *peer_id,
                            handler: NotifyHandler::One(*connection_id),
                            event: handler::In::EstablishCircuit {
                                send_back,
                                dst_peer_id,
                            },
                        });
                }
            }
        }
    }

    fn inject_dial_failure(
        &mut self,
        peer_id: Option<PeerId>,
        _handler: handler::Prototype,
        _error: &DialError,
    ) {
        if let Some(peer_id) = peer_id {
            self.rqsts_pending_connection.remove(&peer_id);
        }
    }

    fn inject_disconnected(&mut self, _peer: &PeerId) {}

    fn inject_connection_closed(
        &mut self,
        peer_id: &PeerId,
        connection_id: &ConnectionId,
        _: &ConnectedPoint,
        _handler: handler::Handler,
    ) {
        self.connected_peers.get_mut(peer_id).map(|cs| {
            cs.remove(
                cs.iter()
                    .position(|c| c == connection_id)
                    .expect("Connection to be known."),
            )
        });
    }

    fn inject_event(
        &mut self,
        event_source: PeerId,
        _connection: ConnectionId,
        handler_event: handler::Event,
    ) {
        match handler_event {
            handler::Event::ReservationReqAccepted { renewal } => {
                self.queued_actions
                    .push_back(NetworkBehaviourAction::GenerateEvent(
                        Event::ReservationReqAccepted {
                            relay_peer_id: event_source,
                            renewal,
                        },
                    ))
            }
            handler::Event::ReservationReqFailed { renewal } => {
                self.queued_actions
                    .push_back(NetworkBehaviourAction::GenerateEvent(
                        Event::ReservationReqFailed {
                            relay_peer_id: event_source,
                            renewal,
                        },
                    ))
            }
            handler::Event::OutboundCircuitReqFailed {} => {
                self.queued_actions
                    .push_back(NetworkBehaviourAction::GenerateEvent(
                        Event::OutboundCircuitReqFailed {
                            relay_peer_id: event_source,
                        },
                    ))
            }
            handler::Event::InboundCircuitReqDenied { src_peer_id } => self
                .queued_actions
                .push_back(NetworkBehaviourAction::GenerateEvent(
                    Event::InboundCircuitReqDenied { src_peer_id },
                )),
            handler::Event::InboundCircuitReqDenyFailed { src_peer_id, error } => self
                .queued_actions
                .push_back(NetworkBehaviourAction::GenerateEvent(
                    Event::InboundCircuitReqDenyFailed { src_peer_id, error },
                )),
        }
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
        _poll_parameters: &mut impl PollParameters,
    ) -> Poll<NetworkBehaviourAction<Self::OutEvent, Self::ProtocolsHandler>> {
        if let Some(event) = self.queued_actions.pop_front() {
            return Poll::Ready(event);
        }

        loop {
            match self.from_transport.poll_next_unpin(cx) {
                Poll::Ready(Some(transport::TransportToBehaviourMsg::ListenReq {
                    relay_peer_id,
                    relay_addr,
                    to_listener,
                })) => {
                    match self
                        .connected_peers
                        .get(&relay_peer_id)
                        .and_then(|cs| cs.get(0))
                    {
                        Some(connection_id) => {
                            return Poll::Ready(NetworkBehaviourAction::NotifyHandler {
                                peer_id: relay_peer_id,
                                handler: NotifyHandler::One(*connection_id),
                                event: handler::In::Reserve { to_listener },
                            });
                        }
                        None => {
                            self.rqsts_pending_connection
                                .entry(relay_peer_id)
                                .or_default()
                                .push(RqstPendingConnection::Reservation { to_listener });
                            let handler = self.new_handler();
                            return Poll::Ready(NetworkBehaviourAction::Dial {
                                opts: DialOpts::peer_id(relay_peer_id)
                                    .addresses(vec![relay_addr])
                                    .extend_addresses_through_behaviour()
                                    .build(),
                                handler,
                            });
                        }
                    }
                }
                Poll::Ready(Some(transport::TransportToBehaviourMsg::DialReq {
                    relay_addr,
                    relay_peer_id,
                    dst_peer_id,
                    send_back,
                    ..
                })) => {
                    match self
                        .connected_peers
                        .get(&relay_peer_id)
                        .and_then(|cs| cs.get(0))
                    {
                        Some(connection_id) => {
                            return Poll::Ready(NetworkBehaviourAction::NotifyHandler {
                                peer_id: relay_peer_id,
                                handler: NotifyHandler::One(*connection_id),
                                event: handler::In::EstablishCircuit {
                                    send_back,
                                    dst_peer_id,
                                },
                            });
                        }
                        None => {
                            self.rqsts_pending_connection
                                .entry(relay_peer_id)
                                .or_default()
                                .push(RqstPendingConnection::Circuit {
                                    dst_peer_id,
                                    send_back,
                                });
                            let handler = self.new_handler();
                            return Poll::Ready(NetworkBehaviourAction::Dial {
                                opts: DialOpts::peer_id(relay_peer_id)
                                    .addresses(vec![relay_addr])
                                    .extend_addresses_through_behaviour()
                                    .build(),
                                handler,
                            });
                        }
                    }
                }
                Poll::Ready(None) => unreachable!(
                    "`Relay` `NetworkBehaviour` polled after channel from \
                     `RelayTransport` has been closed. Unreachable under \
                     the assumption that the `Client` is never polled after \
                     `ClientTransport` is dropped.",
                ),
                Poll::Pending => break,
            }
        }

        Poll::Pending
    }
}

/// A [`NegotiatedSubstream`] acting as a [`RelayedConnection`].
pub enum RelayedConnection {
    InboundAccepting {
        accept: BoxFuture<'static, Result<RelayedConnection, std::io::Error>>,
    },
    Operational {
        read_buffer: Bytes,
        substream: NegotiatedSubstream,
        drop_notifier: oneshot::Sender<void::Void>,
    },
}

impl Unpin for RelayedConnection {}

impl RelayedConnection {
    pub(crate) fn new_inbound(
        circuit: inbound_stop::Circuit,
        drop_notifier: oneshot::Sender<void::Void>,
    ) -> Self {
        RelayedConnection::InboundAccepting {
            accept: async {
                let (substream, read_buffer) = circuit.accept().await?;
                Ok(RelayedConnection::Operational {
                    read_buffer,
                    substream,
                    drop_notifier,
                })
            }
            .boxed(),
        }
    }

    pub(crate) fn new_outbound(
        substream: NegotiatedSubstream,
        read_buffer: Bytes,
        drop_notifier: oneshot::Sender<void::Void>,
    ) -> Self {
        RelayedConnection::Operational {
            substream,
            read_buffer,
            drop_notifier,
        }
    }
}

impl AsyncWrite for RelayedConnection {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        buf: &[u8],
    ) -> Poll<Result<usize, Error>> {
        loop {
            match self.deref_mut() {
                RelayedConnection::InboundAccepting { accept } => {
                    *self = ready!(accept.poll_unpin(cx))?;
                }
                RelayedConnection::Operational { substream, .. } => {
                    return Pin::new(substream).poll_write(cx, buf);
                }
            }
        }
    }
    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Error>> {
        loop {
            match self.deref_mut() {
                RelayedConnection::InboundAccepting { accept } => {
                    *self = ready!(accept.poll_unpin(cx))?;
                }
                RelayedConnection::Operational { substream, .. } => {
                    return Pin::new(substream).poll_flush(cx);
                }
            }
        }
    }
    fn poll_close(mut self: Pin<&mut Self>, cx: &mut Context) -> Poll<Result<(), Error>> {
        loop {
            match self.deref_mut() {
                RelayedConnection::InboundAccepting { accept } => {
                    *self = ready!(accept.poll_unpin(cx))?;
                }
                RelayedConnection::Operational { substream, .. } => {
                    return Pin::new(substream).poll_close(cx);
                }
            }
        }
    }

    fn poll_write_vectored(
        mut self: Pin<&mut Self>,
        cx: &mut Context,
        bufs: &[IoSlice],
    ) -> Poll<Result<usize, Error>> {
        loop {
            match self.deref_mut() {
                RelayedConnection::InboundAccepting { accept } => {
                    *self = ready!(accept.poll_unpin(cx))?;
                }
                RelayedConnection::Operational { substream, .. } => {
                    return Pin::new(substream).poll_write_vectored(cx, bufs);
                }
            }
        }
    }
}

impl AsyncRead for RelayedConnection {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, Error>> {
        loop {
            match self.deref_mut() {
                RelayedConnection::InboundAccepting { accept } => {
                    *self = ready!(accept.poll_unpin(cx))?;
                }
                RelayedConnection::Operational {
                    read_buffer,
                    substream,
                    ..
                } => {
                    if !read_buffer.is_empty() {
                        let n = std::cmp::min(read_buffer.len(), buf.len());
                        let data = read_buffer.split_to(n);
                        buf[0..n].copy_from_slice(&data[..]);
                        return Poll::Ready(Ok(n));
                    }

                    return Pin::new(substream).poll_read(cx, buf);
                }
            }
        }
    }
}

enum RqstPendingConnection {
    Reservation {
        to_listener: Sender<transport::ToListenerMsg>,
    },
    Circuit {
        dst_peer_id: PeerId,
        send_back: oneshot::Sender<Result<RelayedConnection, ()>>,
    },
}