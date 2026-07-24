use std::{mem, str};

use bytes::Bytes;
use serde_json::Value;

use super::ProtocolError;

pub const DEFAULT_MAX_SSE_FRAME_BYTES: usize = 1024 * 1024;
pub const DEFAULT_MAX_SSE_FRAMES_PER_PUSH: usize = 4096;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SseFrame {
    pub event: Option<String>,
    pub data: String,
}

pub struct SseDecoder {
    max_frame_bytes: usize,
    max_frames_per_push: usize,
    frame_bytes: usize,
    line: Vec<u8>,
    event: Option<String>,
    data: String,
    saw_data: bool,
    at_stream_start: bool,
    failed: bool,
    skip_lf: bool,
    count_skipped_lf: bool,
}

impl SseDecoder {
    pub fn new(max_frame_bytes: usize) -> Self {
        Self::with_limits(max_frame_bytes, DEFAULT_MAX_SSE_FRAMES_PER_PUSH)
    }

    pub fn with_limits(max_frame_bytes: usize, max_frames_per_push: usize) -> Self {
        Self {
            max_frame_bytes,
            max_frames_per_push,
            frame_bytes: 0,
            line: Vec::new(),
            event: None,
            data: String::new(),
            saw_data: false,
            at_stream_start: true,
            failed: false,
            skip_lf: false,
            count_skipped_lf: false,
        }
    }

    pub fn push(&mut self, chunk: &[u8]) -> Result<Vec<SseFrame>, ProtocolError> {
        if self.failed {
            return Err(ProtocolError::DecoderFailed);
        }

        match self.push_inner(chunk) {
            Ok(frames) => Ok(frames),
            Err(error) => {
                self.failed = true;
                Err(error)
            }
        }
    }

    pub fn finish(&mut self) -> Result<Vec<SseFrame>, ProtocolError> {
        if self.failed {
            return Err(ProtocolError::DecoderFailed);
        }
        self.failed = true;
        if !self.line.is_empty()
            || self.event.is_some()
            || !self.data.is_empty()
            || self.saw_data
            || self.frame_bytes != 0
        {
            return Err(ProtocolError::UnexpectedEof);
        }
        Ok(Vec::new())
    }

    fn push_inner(&mut self, chunk: &[u8]) -> Result<Vec<SseFrame>, ProtocolError> {
        let mut frames = Vec::new();

        for &byte in chunk {
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    if self.count_skipped_lf {
                        self.add_frame_bytes(1)?;
                    }
                    self.count_skipped_lf = false;
                    continue;
                }
                self.count_skipped_lf = false;
            }

            match byte {
                b'\r' => {
                    let line_has_bytes = !self.line.is_empty();
                    if line_has_bytes {
                        self.add_frame_bytes(1)?;
                    }
                    self.process_line(&mut frames)?;
                    self.skip_lf = true;
                    self.count_skipped_lf = line_has_bytes;
                }
                b'\n' => {
                    if !self.line.is_empty() {
                        self.add_frame_bytes(1)?;
                    }
                    self.process_line(&mut frames)?;
                }
                _ => {
                    self.add_frame_bytes(1)?;
                    self.line.push(byte);
                }
            }
        }

        Ok(frames)
    }

    fn add_frame_bytes(&mut self, added: usize) -> Result<(), ProtocolError> {
        self.frame_bytes = self.frame_bytes.saturating_add(added);
        if self.frame_bytes > self.max_frame_bytes {
            return Err(ProtocolError::FrameTooLarge {
                limit: self.max_frame_bytes,
            });
        }
        Ok(())
    }

    fn process_line(&mut self, frames: &mut Vec<SseFrame>) -> Result<(), ProtocolError> {
        let line = mem::take(&mut self.line);
        let line = if self.at_stream_start {
            self.at_stream_start = false;
            line.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&line)
        } else {
            &line
        };
        let line = str::from_utf8(line).map_err(|_| ProtocolError::InvalidUtf8)?;

        if line.is_empty() {
            if self.saw_data {
                if frames.len() >= self.max_frames_per_push {
                    return Err(ProtocolError::TooManyFrames {
                        limit: self.max_frames_per_push,
                    });
                }
                frames.push(SseFrame {
                    event: self.event.take(),
                    data: mem::take(&mut self.data),
                });
            } else {
                self.event = None;
                self.data.clear();
            }
            self.saw_data = false;
            self.frame_bytes = 0;
            return Ok(());
        }

        if line.starts_with(':') {
            return Ok(());
        }

        let (field, value) = match line.split_once(':') {
            Some((field, value)) => (field, value.strip_prefix(' ').unwrap_or(value)),
            None => (line, ""),
        };
        match field {
            "event" => self.event = Some(value.to_owned()),
            "data" => {
                if self.saw_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.saw_data = true;
            }
            _ => {}
        }

        Ok(())
    }
}

impl Default for SseDecoder {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_SSE_FRAME_BYTES)
    }
}

pub fn encode_sse(event: Option<&str>, data: &Value) -> Bytes {
    let json = serde_json::to_vec(data).expect("serializing a JSON value cannot fail");
    let mut encoded = Vec::with_capacity(json.len() + event.map_or(0, |name| name.len() + 8) + 8);

    if let Some(event) = event.filter(|name| !name.contains(['\r', '\n'])) {
        encoded.extend_from_slice(b"event: ");
        encoded.extend_from_slice(event.as_bytes());
        encoded.push(b'\n');
    }
    encoded.extend_from_slice(b"data: ");
    encoded.extend_from_slice(&json);
    encoded.extend_from_slice(b"\n\n");

    Bytes::from(encoded)
}
