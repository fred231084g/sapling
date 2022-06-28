/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This software may be used and distributed according to the terms of the
 * GNU General Public License version 2.
 */

//! Scaffolding for encoding bundle2 parts.

use std::fmt::Debug;
use std::fmt::Formatter;
use std::fmt::{self};
use std::mem;

use anyhow::Error;
use anyhow::Result;
use bytes::Bytes as BytesNew;
use bytes_old::Bytes;
use futures_ext::BoxStream;
use futures_ext::StreamExt;
use futures_old::Async;
use futures_old::Future;
use futures_old::Poll;
use futures_old::Stream;

use crate::chunk::Chunk;
use crate::part_header::PartHeader;
use crate::part_header::PartHeaderBuilder;
use crate::part_header::PartHeaderType;
use crate::part_header::PartId;

/// Represents a stream of chunks produced by the individual part handler.
pub struct ChunkStream(BoxStream<Chunk, Error>);

impl Debug for ChunkStream {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        f.debug_tuple("ChunkStream").finish()
    }
}

#[derive(Debug)]
pub enum PartEncodeData {
    None,
    Fixed(Chunk),
    Generated(ChunkStream),
}

pub struct PartEncodeBuilder {
    headerb: PartHeaderBuilder,
    data: PartEncodeData,
}

#[derive(Debug)]
pub struct PartEncode {
    state: GenerationState,
}

#[derive(Debug)]
enum GenerationState {
    NotStarted(PartHeader, PartEncodeData),
    Fixed(Chunk),
    Generating(ChunkStream),
    EmptyChunk,
    Done,
    Invalid,
}

impl GenerationState {
    fn take(&mut self) -> Self {
        mem::replace(self, GenerationState::Invalid)
    }
}

impl PartEncodeBuilder {
    pub fn mandatory(part_type: PartHeaderType) -> Result<Self> {
        Ok(PartEncodeBuilder {
            headerb: PartHeaderBuilder::new(part_type, true)?,
            data: PartEncodeData::None,
        })
    }

    pub fn advisory(part_type: PartHeaderType) -> Result<Self> {
        Ok(PartEncodeBuilder {
            headerb: PartHeaderBuilder::new(part_type, false)?,
            data: PartEncodeData::None,
        })
    }

    #[inline]
    pub fn add_mparam<S, B>(&mut self, key: S, val: B) -> Result<&mut Self>
    where
        S: Into<String>,
        B: Into<BytesNew>,
    {
        self.headerb.add_mparam(key, val)?;
        Ok(self)
    }

    #[inline]
    pub fn add_aparam<S, B>(&mut self, key: S, val: B) -> Result<&mut Self>
    where
        S: Into<String>,
        B: Into<BytesNew>,
    {
        self.headerb.add_aparam(key, val)?;
        Ok(self)
    }

    pub fn set_data_fixed<T: Into<Chunk>>(&mut self, data: T) -> &mut Self {
        self.data = PartEncodeData::Fixed(data.into());
        self
    }

    pub fn set_data_bytes<T: Into<Bytes>>(&mut self, data: T) -> Result<&mut Self> {
        self.data = PartEncodeData::Fixed(Chunk::new(data.into())?);
        Ok(self)
    }

    pub fn set_data_future<T>(&mut self, future: T) -> &mut Self
    where
        T: Future<Error = Error> + Send + 'static,
        T::Item: Into<Bytes>,
    {
        let stream = future.and_then(Chunk::new).into_stream();
        self.set_data_generated(stream)
    }

    pub fn set_data_generated<S>(&mut self, stream: S) -> &mut Self
    where
        S: 'static + Stream<Item = Chunk, Error = Error> + Send,
    {
        let stream = stream.filter(|chunk| !chunk.is_empty());
        self.data = PartEncodeData::Generated(ChunkStream(stream.boxify()));
        self
    }

    pub fn build(self, part_id: PartId) -> PartEncode {
        PartEncode {
            state: GenerationState::NotStarted(self.headerb.build(part_id), self.data),
        }
    }
}

impl Stream for PartEncode {
    type Item = Chunk;
    type Error = Error;

    fn poll(&mut self) -> Poll<Option<Chunk>, Error> {
        let (ret, next_state) = Self::poll_next(self.state.take());
        self.state = next_state;
        ret
    }
}

impl PartEncode {
    fn poll_next(state: GenerationState) -> (Poll<Option<Chunk>, Error>, GenerationState) {
        // An individual part has three sections:
        // (1) a header (1 chunk)
        // (2) the payload (0-many chunks)
        // (3) an empty chunk, indicating the end of the stream.
        //
        // The state machine captures the generation as:
        // NotStarted = header not output yet
        // Generating = payload currently being generated by inner stream
        // Fixed = fixed-length payload (no generation, just one chunk)
        // EmptyChunk = end of payload (or no payload)
        // Done = chunk completed
        // Invalid = some sort of error occured
        use self::GenerationState::*;

        match state {
            NotStarted(header, data) => {
                let header_chunk = header.encode();
                let next_state = match data {
                    PartEncodeData::Fixed(b) => Fixed(b),
                    PartEncodeData::None => EmptyChunk,
                    PartEncodeData::Generated(ChunkStream(stream)) => {
                        Generating(ChunkStream(stream))
                    }
                };
                (Ok(Async::Ready(Some(header_chunk))), next_state)
            }
            Generating(ChunkStream(mut stream)) => {
                match stream.poll() {
                    Ok(Async::Ready(Some(v))) => {
                        // TODO: don't send too large or too small chunks to clients
                        (Ok(Async::Ready(Some(v))), Generating(ChunkStream(stream)))
                    }
                    Ok(Async::Ready(None)) => (Ok(Async::Ready(Some(Chunk::empty()))), Done),
                    Ok(Async::NotReady) => (Ok(Async::NotReady), Generating(ChunkStream(stream))),
                    // TODO: produce an error part for (some kinds of?) errors
                    Err(e) => (Err(e), Generating(ChunkStream(stream))),
                }
            }
            Fixed(chunk) => (Ok(Async::Ready(Some(chunk))), EmptyChunk),
            EmptyChunk => (Ok(Async::Ready(Some(Chunk::empty()))), Done),
            Done => (Ok(Async::Ready(None)), Done),
            Invalid => panic!("invalid state"),
        }
    }
}
