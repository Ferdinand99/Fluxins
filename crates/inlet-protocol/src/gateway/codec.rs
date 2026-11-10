//! `zstd-stream` transport compression.
//!
//! With `compress=zstd-stream&stream=1`, every server frame is binary and all
//! frames of one connection form one zstd stream; every client message goes
//! through one compressor for the whole connection, flushed per message.

use std::io::Write;

/// One connection's compressor and decompressor.
pub struct ZstdStream {
    encoder: zstd::stream::write::Encoder<'static, Vec<u8>>,
    decoder: zstd::stream::write::Decoder<'static, Vec<u8>>,
}

impl ZstdStream {
    /// New codec at compression level 3, as the official client uses.
    pub fn new() -> std::io::Result<Self> {
        Ok(Self {
            encoder: zstd::stream::write::Encoder::new(Vec::new(), 3)?,
            decoder: zstd::stream::write::Decoder::new(Vec::new())?,
        })
    }

    /// Compresses one message and flushes, so the peer can decode it at once.
    pub fn compress(&mut self, data: &[u8]) -> std::io::Result<Vec<u8>> {
        self.encoder.write_all(data)?;
        self.encoder.flush()?;
        Ok(std::mem::take(self.encoder.get_mut()))
    }

    /// Feeds one binary frame and returns the bytes it completes.
    pub fn decompress(&mut self, frame: &[u8]) -> std::io::Result<Vec<u8>> {
        self.decoder.write_all(frame)?;
        self.decoder.flush()?;
        Ok(std::mem::take(self.decoder.get_mut()))
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
                "x".repeat(i * 50)
            );
            let frame = a.compress(msg.as_bytes()).unwrap();
            assert!(!frame.is_empty());
            let out = b.decompress(&frame).unwrap();
            assert_eq!(out, msg.as_bytes());
        }
    }
}
