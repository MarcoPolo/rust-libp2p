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

use crate::v2::message_proto::{stop_message, Status, StopMessage};
use crate::v2::protocol::{MAX_MESSAGE_SIZE, STOP_PROTOCOL_NAME};
use asynchronous_codec::{Framed, FramedParts};
use bytes::{Bytes, BytesMut};
use futures::{future::BoxFuture, prelude::*};
use libp2p_core::{upgrade, PeerId};
use libp2p_swarm::NegotiatedSubstream;
use prost::Message;
use std::io::Cursor;
use std::iter;
use thiserror::Error;
use unsigned_varint::codec::UviBytes;

pub struct Upgrade {}

impl upgrade::UpgradeInfo for Upgrade {
    type Info = &'static [u8];
    type InfoIter = iter::Once<Self::Info>;

    fn protocol_info(&self) -> Self::InfoIter {
        iter::once(STOP_PROTOCOL_NAME)
    }
}

impl upgrade::InboundUpgrade<NegotiatedSubstream> for Upgrade {
    type Output = Circuit;
    type Error = UpgradeError;
    type Future = BoxFuture<'static, Result<Self::Output, Self::Error>>;

    fn upgrade_inbound(self, substream: NegotiatedSubstream, _: Self::Info) -> Self::Future {
        let mut codec = UviBytes::<bytes::Bytes>::default();
        codec.set_max_len(MAX_MESSAGE_SIZE);
        let mut substream = Framed::new(substream, codec);

        async move {
            let msg: bytes::BytesMut = substream
                .next()
                .await
                .ok_or(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, ""))??;

            let StopMessage {
                r#type,
                peer,
                limit: _,
                status: _,
            } = StopMessage::decode(Cursor::new(msg))?;

            let r#type =
                stop_message::Type::from_i32(r#type).ok_or(UpgradeError::ParseTypeField)?;
            match r#type {
                stop_message::Type::Connect => {
                    let src_peer_id =
                        PeerId::from_bytes(&peer.ok_or(UpgradeError::MissingPeer)?.id)
                            .map_err(|_| UpgradeError::ParsePeerId)?;
                    Ok(Circuit {
                        substream,
                        src_peer_id,
                    })
                }
                stop_message::Type::Status => Err(UpgradeError::UnexpectedTypeStatus),
            }
        }
        .boxed()
    }
}

#[derive(Debug, Error)]
pub enum UpgradeError {
    #[error("Failed to decode message: {0}.")]
    Decode(
        #[from]
        #[source]
        prost::DecodeError,
    ),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error("Failed to parse response type field.")]
    ParseTypeField,
    #[error("Failed to parse peer id.")]
    ParsePeerId,
    #[error("Expected 'peer' field to be set.")]
    MissingPeer,
    #[error("Unexpected message type 'status'")]
    UnexpectedTypeStatus,
}

pub struct Circuit {
    substream: Framed<NegotiatedSubstream, UviBytes>,
    src_peer_id: PeerId,
}

impl Circuit {
    pub(crate) fn src_peer_id(&self) -> PeerId {
        self.src_peer_id
    }

    pub async fn accept(mut self) -> Result<(NegotiatedSubstream, Bytes), std::io::Error> {
        let msg = StopMessage {
            r#type: stop_message::Type::Status.into(),
            peer: None,
            limit: None,
            status: Some(Status::Ok.into()),
        };

        self.send(msg).await?;

        let FramedParts {
            io,
            read_buffer,
            write_buffer,
            ..
        } = self.substream.into_parts();
        assert!(
            write_buffer.is_empty(),
            "Expect a flushed Framed to have an empty write buffer."
        );

        Ok((io, read_buffer.freeze()))
    }

    pub async fn deny(mut self, status: Status) -> Result<(), std::io::Error> {
        let msg = StopMessage {
            r#type: stop_message::Type::Status.into(),
            peer: None,
            limit: None,
            status: Some(status.into()),
        };

        self.send(msg).await
    }

    async fn send(&mut self, msg: StopMessage) -> Result<(), std::io::Error> {
        let mut msg_bytes = BytesMut::new();
        msg.encode(&mut msg_bytes)
            .expect("BytesMut to have sufficient capacity.");
        self.substream.send(msg_bytes.freeze()).await?;
        self.substream.flush().await?;

        Ok(())
    }
}
