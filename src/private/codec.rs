use crate::error::{Error, ErrorKind, Result};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufWriter};

#[derive(Clone, Debug)]
pub(crate) struct EncodedFrame {
    bytes: Vec<u8>,
}

impl EncodedFrame {
    pub(crate) fn encode(value: &Value, max_frame_bytes: usize) -> Result<Self> {
        let mut bytes = serde_json::to_vec(value)
            .map_err(|error| Error::new(ErrorKind::Protocol, "jsonl.write", error.to_string()))?;
        if bytes.len() > max_frame_bytes {
            return Err(Error::new(
                ErrorKind::Protocol,
                "jsonl.write",
                format!("frame exceeds {max_frame_bytes} bytes"),
            ));
        }
        bytes.push(b'\n');
        Ok(Self { bytes })
    }

    fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }
}

pub(crate) struct JsonLinesReader<R> {
    reader: R,
    buffer: Vec<u8>,
    start: usize,
    max_frame_bytes: usize,
    eof: bool,
}

impl<R: AsyncRead + Unpin> JsonLinesReader<R> {
    pub(crate) fn new(reader: R, max_frame_bytes: usize) -> Self {
        Self {
            reader,
            buffer: Vec::with_capacity(8192),
            start: 0,
            max_frame_bytes,
            eof: false,
        }
    }

    pub(crate) async fn next(&mut self) -> Result<Option<Value>> {
        loop {
            if let Some(relative) = self.buffer[self.start..]
                .iter()
                .position(|byte| *byte == b'\n')
            {
                let position = self.start + relative;
                let mut end = position;
                if end > self.start && self.buffer[end - 1] == b'\r' {
                    end -= 1;
                }
                let frame = &self.buffer[self.start..end];
                if frame.len() > self.max_frame_bytes {
                    return Err(frame_too_large("jsonl.read", self.max_frame_bytes));
                }
                if frame.is_empty() {
                    return Err(Error::new(
                        ErrorKind::Protocol,
                        "jsonl.read",
                        "empty JSONL frames are not allowed",
                    ));
                }
                let parsed = parse_frame(frame);
                self.start = position + 1;
                if self.start == self.buffer.len() {
                    self.buffer.clear();
                    self.start = 0;
                }
                return parsed.map(Some);
            }
            let unread = self.buffer.len() - self.start;
            if unread > self.max_frame_bytes.saturating_add(1) {
                return Err(frame_too_large("jsonl.read", self.max_frame_bytes));
            }
            if self.eof {
                if unread == 0 {
                    return Ok(None);
                }
                return Err(Error::new(
                    ErrorKind::Protocol,
                    "jsonl.read",
                    "EOF occurred in the middle of a JSONL frame",
                ));
            }
            if self.start > 0 {
                self.buffer.copy_within(self.start.., 0);
                self.buffer.truncate(unread);
                self.start = 0;
            }
            let remaining = self
                .max_frame_bytes
                .saturating_add(2)
                .saturating_sub(self.buffer.len());
            let chunk_size = remaining.clamp(1, 8192);
            let start = self.buffer.len();
            self.buffer.resize(start + chunk_size, 0);
            let count = self
                .reader
                .read(&mut self.buffer[start..])
                .await
                .map_err(|error| Error::new(ErrorKind::Process, "jsonl.read", error.to_string()))?;
            self.buffer.truncate(start + count);
            if count == 0 {
                self.eof = true;
            }
        }
    }
}

pub(crate) struct JsonLinesWriter<W> {
    writer: BufWriter<W>,
}

impl<W: AsyncWrite + Unpin> JsonLinesWriter<W> {
    pub(crate) fn new(writer: W) -> Self {
        Self {
            writer: BufWriter::new(writer),
        }
    }

    pub(crate) async fn write(&mut self, frame: &EncodedFrame) -> Result<()> {
        self.writer
            .write_all(frame.as_bytes())
            .await
            .map_err(|error| Error::new(ErrorKind::Process, "jsonl.write", error.to_string()))?;
        self.writer
            .flush()
            .await
            .map_err(|error| Error::new(ErrorKind::Process, "jsonl.flush", error.to_string()))
    }

    pub(crate) async fn close(&mut self) -> Result<()> {
        self.writer
            .shutdown()
            .await
            .map_err(|error| Error::new(ErrorKind::Process, "jsonl.close", error.to_string()))
    }
}

fn frame_too_large(operation: &'static str, max_frame_bytes: usize) -> Error {
    Error::new(
        ErrorKind::Protocol,
        operation,
        format!("frame exceeds {max_frame_bytes} bytes"),
    )
}

fn parse_frame(frame: &[u8]) -> Result<Value> {
    let text = std::str::from_utf8(frame).map_err(|_| {
        Error::new(
            ErrorKind::Protocol,
            "jsonl.read",
            "frame was not valid UTF-8",
        )
    })?;
    serde_json::from_str(text).map_err(|error| {
        Error::new(
            ErrorKind::Protocol,
            "jsonl.read",
            format!("invalid JSON frame: {error}"),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::fmt::Write as _;

    #[tokio::test]
    async fn accepts_frame_larger_than_64_kib() {
        let (mut input, output) = tokio::io::duplex(256 * 1024);
        let payload = "x".repeat(70 * 1024);
        let task = tokio::spawn(async move {
            input
                .write_all(format!("{{\"value\":\"{payload}\"}}\n").as_bytes())
                .await
                .expect("write");
        });
        let mut reader = JsonLinesReader::new(output, 128 * 1024);
        let value = reader.next().await.expect("read").expect("frame");
        assert_eq!(value["value"].as_str().expect("string").len(), 70 * 1024);
        task.await.expect("writer");
    }

    #[tokio::test]
    async fn accepts_exact_limit_and_crlf() {
        let value = json!({"v": "x"});
        let encoded = serde_json::to_vec(&value).expect("encode");
        let limit = encoded.len();
        let (mut input, output) = tokio::io::duplex(128);
        let task = tokio::spawn(async move {
            input.write_all(&encoded).await.expect("write frame");
            input.write_all(b"\r\n").await.expect("write terminator");
        });
        let mut reader = JsonLinesReader::new(output, limit);
        assert_eq!(reader.next().await.expect("read"), Some(value));
        task.await.expect("writer");
    }

    #[tokio::test]
    async fn rejects_empty_frame() {
        let (mut input, output) = tokio::io::duplex(16);
        let task = tokio::spawn(async move {
            input.write_all(b"\n").await.expect("write");
        });
        let mut reader = JsonLinesReader::new(output, 1024);
        let error = reader.next().await.expect_err("must reject");
        assert_eq!(error.kind(), ErrorKind::Protocol);
        task.await.expect("writer");
    }

    #[tokio::test]
    async fn rejects_oversized_frame_without_newline() {
        let (mut input, output) = tokio::io::duplex(4096);
        let task = tokio::spawn(async move {
            input.write_all(&vec![b'x'; 2048]).await.expect("write");
        });
        let mut reader = JsonLinesReader::new(output, 1024);
        let error = reader.next().await.expect_err("must reject");
        assert_eq!(error.kind(), ErrorKind::Protocol);
        task.await.expect("writer");
    }

    #[tokio::test]
    async fn rejects_oversized_frame_when_newline_arrives_in_same_read() {
        let (mut input, output) = tokio::io::duplex(4096);
        let task = tokio::spawn(async move {
            let mut bytes = vec![b'x'; 1025];
            bytes.push(b'\n');
            input.write_all(&bytes).await.expect("write");
        });
        let mut reader = JsonLinesReader::new(output, 1024);
        let error = reader.next().await.expect_err("must reject");
        assert_eq!(error.kind(), ErrorKind::Protocol);
        assert!(error.message().contains("1024"));
        task.await.expect("writer");
    }

    #[tokio::test]
    async fn reads_many_frames_without_front_draining_the_buffer() {
        let mut frames = String::new();
        for index in 0..1_000 {
            writeln!(frames, "{{\"index\":{index}}}").expect("append frame");
        }
        let frames = frames.into_bytes();
        let mut reader = JsonLinesReader::new(std::io::Cursor::new(frames), 1024);
        for index in 0..1_000 {
            let value = reader.next().await.expect("read").expect("frame");
            assert_eq!(value["index"], index);
        }
        assert_eq!(reader.next().await.expect("EOF"), None);
    }

    #[test]
    fn encoded_frame_is_validated_before_dispatch() {
        let value = json!({"value": "x".repeat(128)});
        let error = EncodedFrame::encode(&value, 16).expect_err("must reject");
        assert_eq!(error.kind(), ErrorKind::Protocol);
    }

    #[tokio::test]
    async fn writer_emits_one_preencoded_jsonl_frame() {
        let (output, mut input) = tokio::io::duplex(128);
        let frame = EncodedFrame::encode(&json!({"value": 7}), 128).expect("frame");
        let mut writer = JsonLinesWriter::new(output);
        writer.write(&frame).await.expect("write");
        let mut bytes = vec![0; frame.as_bytes().len()];
        input.read_exact(&mut bytes).await.expect("read");
        assert_eq!(bytes, b"{\"value\":7}\n");
    }
}
