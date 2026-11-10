//! `zstd-stream` transport compression.
//!
//! With `compress=zstd-stream&stream=1`, every server frame is binary and all
//! frames of one connection form one zstd stream; every client message goes
//! through one compressor for the whole connection, flushed per message.
//!
//! Decompression is bounded: one message may not grow past the configured
//! limit, and the zstd window is capped at 128 MiB (`window_log_max` 27), so a
//! small "decompression bomb" frame cannot exhaust memory.

use std::io::{self, Write};

use zstd::stream::raw::{DParameter, Decoder, InBuffer, Operation, OutBuffer};

/// Default limit for one decompressed gateway message.
pub const DEFAULT_MAX_MESSAGE: usize = 32 * 1024 * 1024;

const CHUNK: usize = 64 * 1024;

/// One connection's compressor and decompressor.
pub struct ZstdStream {
    encoder: zstd::stream::write::Encoder<'static, Vec<u8>>,
    decoder: Decoder<'static>,
    chunk: Vec<u8>,
    max_output: usize,
}

impl ZstdStream {
    /// New codec at compression level 3 (as the official client uses), with
    /// the default message limit.
    pub fn new() -> io::Result<Self> {
        Self::with_limit(DEFAULT_MAX_MESSAGE)
    }

    /// New codec that refuses decompressed messages over `max_output` bytes.
    pub fn with_limit(max_output: usize) -> io::Result<Self> {
        let mut decoder = Decoder::new()?;
        decoder.set_parameter(DParameter::WindowLogMax(27))?;
        Ok(Self {
            encoder: zstd::stream::write::Encoder::new(Vec::new(), 3)?,
            decoder,
            chunk: vec![0; CHUNK],
            max_output,
        })
    }

    /// Compresses one message and flushes, so the peer can decode it at once.
    pub fn compress(&mut self, data: &[u8]) -> io::Result<Vec<u8>> {
        self.encoder.write_all(data)?;
        self.encoder.flush()?;
        Ok(std::mem::take(self.encoder.get_mut()))
    }

    /// Feeds one binary frame and returns the bytes it completes. Fails with
    /// `InvalidData` once the output passes the limit; the stream is then
    /// unusable and the connection should be dropped.
    pub fn decompress(&mut self, frame: &[u8]) -> io::Result<Vec<u8>> {
        let mut out = Vec::new();
        let mut input = InBuffer::around(frame);
        loop {
            let written = {
                let mut output = OutBuffer::around(&mut self.chunk[..]);
                self.decoder.run(&mut input, &mut output)?;
                output.pos()
            };
            if out.len() + written > self.max_output {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!(
                        "decompressed gateway message is over {} bytes",
                        self.max_output
                    ),
                ));
            }
            out.extend_from_slice(&self.chunk[..written]);
            if input.pos() >= frame.len() && written < self.chunk.len() {
                return Ok(out);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_roundtrip_across_messages() {
        let mut a = ZstdStream::new().unwrap();
        let mut b = ZstdStream::new().unwrap();
        for i in 0..20 {
            let msg = format!(
                "{{\"op\":0,\"s\":{i},\"d\":{{\"content\":\"{}\"}}}}",
                "x".repeat(i * 5_000)
            );
            let frame = a.compress(msg.as_bytes()).unwrap();
            assert!(!frame.is_empty());
            let out = b.decompress(&frame).unwrap();
            assert_eq!(out, msg.as_bytes());
        }
    }

    #[test]
    fn decompression_bomb_is_refused() {
        let mut server = ZstdStream::new().unwrap();
        let mut client = ZstdStream::with_limit(16 * 1024 * 1024).unwrap();
        // 64 MiB of zeros compresses to a few KiB.
        let bomb = vec![0u8; 64 * 1024 * 1024];
        let frame = server.compress(&bomb).unwrap();
        assert!(frame.len() < 64 * 1024, "{}", frame.len());
        let err = client.decompress(&frame).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }
}
