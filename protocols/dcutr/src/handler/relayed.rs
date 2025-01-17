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

//! [`ProtocolsHandler`] handling relayed connection potentially upgraded to a direct connection.

use crate::protocol;
use futures::future::{BoxFuture, FutureExt};
use futures::stream::{FuturesUnordered, StreamExt};
use instant::Instant;
use libp2p_core::either::{EitherError, EitherOutput};
use libp2p_core::multiaddr::Multiaddr;
use libp2p_core::upgrade::{self, DeniedUpgrade, NegotiationError, UpgradeError};
use libp2p_core::ConnectedPoint;
use libp2p_swarm::protocols_handler::{InboundUpgradeSend, OutboundUpgradeSend};
use libp2p_swarm::{
    KeepAlive, NegotiatedSubstream, ProtocolsHandler, ProtocolsHandlerEvent,
    ProtocolsHandlerUpgrErr, SubstreamProtocol,
};
use std::collections::VecDeque;
use std::fmt;
use std::task::{Context, Poll};
use std::time::Duration;

pub enum Command {
    Connect {
        obs_addrs: Vec<Multiaddr>,
        attempt: u8,
    },
    AcceptInboundConnect {
        obs_addrs: Vec<Multiaddr>,
        inbound_connect: protocol::inbound::PendingConnect,
    },
    /// Upgrading the relayed connection to a direct connection either failed for good or succeeded.
    /// There is no need to keep the relayed connection alive for the sake of upgrading to a direct
    /// connection.
    UpgradeFinishedDontKeepAlive,
}

impl fmt::Debug for Command {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Command::Connect { obs_addrs, attempt } => f
                .debug_struct("Command::Connect")
                .field("obs_addrs", obs_addrs)
                .field("attempt", attempt)
                .finish(),
            Command::AcceptInboundConnect {
                obs_addrs,
                inbound_connect: _,
            } => f
                .debug_struct("Command::AcceptInboundConnect")
                .field("obs_addrs", obs_addrs)
                .finish(),
            Command::UpgradeFinishedDontKeepAlive => f
                .debug_struct("Command::UpgradeFinishedDontKeepAlive")
                .finish(),
        }
    }
}

pub enum Event {
    InboundConnectRequest {
        inbound_connect: protocol::inbound::PendingConnect,
        remote_addr: Multiaddr,
    },
    InboundNegotiationFailed {
        error: ProtocolsHandlerUpgrErr<void::Void>,
    },
    InboundConnectNegotiated(Vec<Multiaddr>),
    OutboundNegotiationFailed {
        error: ProtocolsHandlerUpgrErr<void::Void>,
    },
    OutboundConnectNegotiated {
        remote_addrs: Vec<Multiaddr>,
        attempt: u8,
    },
}

impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Event::InboundConnectRequest {
                inbound_connect: _,
                remote_addr,
            } => f
                .debug_struct("Event::InboundConnectRequest")
                .field("remote_addrs", remote_addr)
                .finish(),
            Event::InboundNegotiationFailed { error } => f
                .debug_struct("Event::InboundNegotiationFailed")
                .field("error", error)
                .finish(),
            Event::InboundConnectNegotiated(addrs) => f
                .debug_tuple("Event::InboundConnectNegotiated")
                .field(addrs)
                .finish(),
            Event::OutboundNegotiationFailed { error } => f
                .debug_struct("Event::OutboundNegotiationFailed")
                .field("error", error)
                .finish(),
            Event::OutboundConnectNegotiated {
                remote_addrs,
                attempt,
            } => f
                .debug_struct("Event::OutboundConnectNegotiated")
                .field("remote_addrs", remote_addrs)
                .field("attempt", attempt)
                .finish(),
        }
    }
}

pub struct Handler {
    endpoint: ConnectedPoint,
    /// A pending fatal error that results in the connection being closed.
    pending_error: Option<
        ProtocolsHandlerUpgrErr<
            EitherError<protocol::inbound::UpgradeError, protocol::outbound::UpgradeError>,
        >,
    >,
    /// Queue of events to return when polled.
    queued_events: VecDeque<
        ProtocolsHandlerEvent<
            <Self as ProtocolsHandler>::OutboundProtocol,
            <Self as ProtocolsHandler>::OutboundOpenInfo,
            <Self as ProtocolsHandler>::OutEvent,
            <Self as ProtocolsHandler>::Error,
        >,
    >,
    /// Inbound connects, accepted by the behaviour, pending completion.
    inbound_connects: FuturesUnordered<
        BoxFuture<'static, Result<Vec<Multiaddr>, protocol::inbound::UpgradeError>>,
    >,
    keep_alive: KeepAlive,
}

impl Handler {
    pub fn new(endpoint: ConnectedPoint) -> Self {
        Self {
            endpoint,
            pending_error: Default::default(),
            queued_events: Default::default(),
            inbound_connects: Default::default(),
            keep_alive: KeepAlive::Until(Instant::now() + Duration::from_secs(30)),
        }
    }
}

impl ProtocolsHandler for Handler {
    type InEvent = Command;
    type OutEvent = Event;
    type Error = ProtocolsHandlerUpgrErr<
        EitherError<protocol::inbound::UpgradeError, protocol::outbound::UpgradeError>,
    >;
    type InboundProtocol = upgrade::EitherUpgrade<protocol::inbound::Upgrade, DeniedUpgrade>;
    type OutboundProtocol = protocol::outbound::Upgrade;
    type OutboundOpenInfo = u8; // Number of upgrade attempts.
    type InboundOpenInfo = ();

    fn listen_protocol(&self) -> SubstreamProtocol<Self::InboundProtocol, Self::InboundOpenInfo> {
        match self.endpoint {
            ConnectedPoint::Dialer { .. } => {
                SubstreamProtocol::new(upgrade::EitherUpgrade::A(protocol::inbound::Upgrade {}), ())
            }
            ConnectedPoint::Listener { .. } => {
                // By the protocol specification the listening side of a relayed connection
                // initiates the _direct connection upgrade_. In other words the listening side of
                // the relayed connection opens a substream to the dialing side. (Connection roles
                // and substream roles are reversed.) The listening side on a relayed connection
                // never expects incoming substreams, hence the denied upgrade below.
                SubstreamProtocol::new(upgrade::EitherUpgrade::B(DeniedUpgrade), ())
            }
        }
    }

    fn inject_fully_negotiated_inbound(
        &mut self,
        output: <Self::InboundProtocol as upgrade::InboundUpgrade<NegotiatedSubstream>>::Output,
        _: Self::InboundOpenInfo,
    ) {
        match output {
            EitherOutput::First(inbound_connect) => {
                let remote_addr = match &self.endpoint {
                    ConnectedPoint::Dialer { address, role_override: _ } => address.clone(),
                    ConnectedPoint::Listener { ..} => unreachable!("`<Handler as ProtocolsHandler>::listen_protocol` denies all incoming substreams as a listener."),
                };
                self.queued_events.push_back(ProtocolsHandlerEvent::Custom(
                    Event::InboundConnectRequest {
                        inbound_connect,
                        remote_addr,
                    },
                ));
            }
            // A connection listener denies all incoming substreams, thus none can ever be fully negotiated.
            EitherOutput::Second(output) => void::unreachable(output),
        }
    }

    fn inject_fully_negotiated_outbound(
        &mut self,
        protocol::outbound::Connect { obs_addrs }: <Self::OutboundProtocol as upgrade::OutboundUpgrade<
            NegotiatedSubstream,
        >>::Output,
        attempt: Self::OutboundOpenInfo,
    ) {
        assert!(
            self.endpoint.is_listener(),
            "A connection dialer never initiates a connection upgrade."
        );
        self.queued_events.push_back(ProtocolsHandlerEvent::Custom(
            Event::OutboundConnectNegotiated {
                remote_addrs: obs_addrs,
                attempt,
            },
        ));
    }

    fn inject_event(&mut self, event: Self::InEvent) {
        match event {
            Command::Connect { obs_addrs, attempt } => {
                self.queued_events
                    .push_back(ProtocolsHandlerEvent::OutboundSubstreamRequest {
                        protocol: SubstreamProtocol::new(
                            protocol::outbound::Upgrade::new(obs_addrs),
                            attempt,
                        ),
                    });
            }
            Command::AcceptInboundConnect {
                inbound_connect,
                obs_addrs,
            } => {
                self.inbound_connects
                    .push(inbound_connect.accept(obs_addrs).boxed());
            }
            Command::UpgradeFinishedDontKeepAlive => {
                self.keep_alive = KeepAlive::No;
            }
        }
    }

    fn inject_listen_upgrade_error(
        &mut self,
        _: Self::InboundOpenInfo,
        error: ProtocolsHandlerUpgrErr<<Self::InboundProtocol as InboundUpgradeSend>::Error>,
    ) {
        match error {
            ProtocolsHandlerUpgrErr::Timeout => {
                self.queued_events.push_back(ProtocolsHandlerEvent::Custom(
                    Event::InboundNegotiationFailed {
                        error: ProtocolsHandlerUpgrErr::Timeout,
                    },
                ));
            }
            ProtocolsHandlerUpgrErr::Timer => {
                self.queued_events.push_back(ProtocolsHandlerEvent::Custom(
                    Event::InboundNegotiationFailed {
                        error: ProtocolsHandlerUpgrErr::Timer,
                    },
                ));
            }
            ProtocolsHandlerUpgrErr::Upgrade(UpgradeError::Select(NegotiationError::Failed)) => {
                // The remote merely doesn't support the DCUtR protocol.
                // This is no reason to close the connection, which may
                // successfully communicate with other protocols already.
                self.keep_alive = KeepAlive::No;
                self.queued_events.push_back(ProtocolsHandlerEvent::Custom(
                    Event::InboundNegotiationFailed {
                        error: ProtocolsHandlerUpgrErr::Upgrade(UpgradeError::Select(
                            NegotiationError::Failed,
                        )),
                    },
                ));
            }
            _ => {
                // Anything else is considered a fatal error or misbehaviour of
                // the remote peer and results in closing the connection.
                self.pending_error = Some(error.map_upgrade_err(|e| {
                    e.map_err(|e| match e {
                        EitherError::A(e) => EitherError::A(e),
                        EitherError::B(v) => void::unreachable(v),
                    })
                }));
            }
        }
    }

    fn inject_dial_upgrade_error(
        &mut self,
        _open_info: Self::OutboundOpenInfo,
        error: ProtocolsHandlerUpgrErr<<Self::OutboundProtocol as OutboundUpgradeSend>::Error>,
    ) {
        self.keep_alive = KeepAlive::No;

        match error {
            ProtocolsHandlerUpgrErr::Timeout => {
                self.queued_events.push_back(ProtocolsHandlerEvent::Custom(
                    Event::OutboundNegotiationFailed {
                        error: ProtocolsHandlerUpgrErr::Timeout,
                    },
                ));
            }
            ProtocolsHandlerUpgrErr::Upgrade(UpgradeError::Select(NegotiationError::Failed)) => {
                // The remote merely doesn't support the DCUtR protocol.
                // This is no reason to close the connection, which may
                // successfully communicate with other protocols already.
                self.queued_events.push_back(ProtocolsHandlerEvent::Custom(
                    Event::OutboundNegotiationFailed {
                        error: ProtocolsHandlerUpgrErr::Upgrade(UpgradeError::Select(
                            NegotiationError::Failed,
                        )),
                    },
                ));
            }
            _ => {
                // Anything else is considered a fatal error or misbehaviour of
                // the remote peer and results in closing the connection.
                self.pending_error =
                    Some(error.map_upgrade_err(|e| e.map_err(|e| EitherError::B(e))));
            }
        }
    }

    fn connection_keep_alive(&self) -> KeepAlive {
        self.keep_alive
    }

    fn poll(
        &mut self,
        cx: &mut Context<'_>,
    ) -> Poll<
        ProtocolsHandlerEvent<
            Self::OutboundProtocol,
            Self::OutboundOpenInfo,
            Self::OutEvent,
            Self::Error,
        >,
    > {
        // Check for a pending (fatal) error.
        if let Some(err) = self.pending_error.take() {
            // The handler will not be polled again by the `Swarm`.
            return Poll::Ready(ProtocolsHandlerEvent::Close(err));
        }

        // Return queued events.
        if let Some(event) = self.queued_events.pop_front() {
            return Poll::Ready(event);
        }

        while let Poll::Ready(Some(result)) = self.inbound_connects.poll_next_unpin(cx) {
            match result {
                Ok(addresses) => {
                    return Poll::Ready(ProtocolsHandlerEvent::Custom(
                        Event::InboundConnectNegotiated(addresses),
                    ));
                }
                Err(e) => {
                    return Poll::Ready(ProtocolsHandlerEvent::Close(
                        ProtocolsHandlerUpgrErr::Upgrade(UpgradeError::Apply(EitherError::A(e))),
                    ))
                }
            }
        }

        Poll::Pending
    }
}
