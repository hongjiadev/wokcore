use std::{future::pending, time::Duration};

use serde_json::json;
use tokio::sync::Semaphore;
use wokcore_protocols::stream::{
    DEFAULT_MAX_SSE_FRAME_BYTES, ProtocolError, ReceiveError, SseDecoder, SseFrame,
    bounded_event_channel, encode_sse,
};

const FIXTURE: &[u8] = include_bytes!("../../../tests/fixtures/protocols/responses/multiline.sse");

fn decode_in_chunks(input: &[u8], chunk_sizes: impl IntoIterator<Item = usize>) -> Vec<SseFrame> {
    let mut decoder = SseDecoder::new(4096);
    let mut frames = Vec::new();
    let mut offset = 0;

    for chunk_size in chunk_sizes {
        if offset == input.len() {
            break;
        }
        let end = (offset + chunk_size.max(1)).min(input.len());
        frames.extend(decoder.push(&input[offset..end]).unwrap());
        offset = end;
    }
    if offset < input.len() {
        frames.extend(decoder.push(&input[offset..]).unwrap());
    }

    frames
}

fn repeated_sse_frames(count: usize) -> Vec<u8> {
    b"data: x\n\n".repeat(count)
}

fn pseudo_random_chunk_sizes(input_len: usize) -> Vec<usize> {
    let mut state = 0x9e37_79b9_u32;
    let mut covered = 0;
    let mut sizes = Vec::new();

    while covered < input_len {
        state ^= state << 13;
        state ^= state >> 17;
        state ^= state << 5;
        let size = (state as usize % 11) + 1;
        sizes.push(size);
        covered += size;
    }

    sizes
}

#[test]
fn decoder_handles_bom_comments_multiline_data_and_fragmented_utf8() {
    let expected = vec![SseFrame {
        event: Some("response.output_text.delta".to_owned()),
        data: "{\"delta\":\n\"你好\"}".to_owned(),
    }];

    assert_eq!(decode_in_chunks(FIXTURE, vec![1; FIXTURE.len()]), expected);
    assert_eq!(
        decode_in_chunks(FIXTURE, pseudo_random_chunk_sizes(FIXTURE.len())),
        expected
    );
}

#[test]
fn decoder_accepts_lf_crlf_and_cr_line_endings() {
    let input = b"event: first\rdata: one\r\r\
                  event: second\r\ndata: two\r\n\r\n\
                  data: three\n\n";

    let frames = decode_in_chunks(input, vec![1; input.len()]);

    assert_eq!(
        frames,
        [
            SseFrame {
                event: Some("first".to_owned()),
                data: "one".to_owned(),
            },
            SseFrame {
                event: Some("second".to_owned()),
                data: "two".to_owned(),
            },
            SseFrame {
                event: None,
                data: "three".to_owned(),
            },
        ]
    );
}

#[test]
fn decoder_follows_field_colon_space_and_dispatch_rules() {
    let input = b"\n\
                  : heartbeat\n\n\
                  event: ignored-without-data\n\n\
                  unknown: ignored\n\
                  event:tick\n\
                  data\n\
                  data: first\n\
                  data:  second\n\n";
    let mut decoder = SseDecoder::new(1024);

    assert_eq!(
        decoder.push(input).unwrap(),
        [SseFrame {
            event: Some("tick".to_owned()),
            data: "\nfirst\n second".to_owned(),
        }]
    );
}

#[test]
fn decoder_emits_only_after_a_blank_line() {
    let mut decoder = SseDecoder::new(1024);

    assert!(
        decoder
            .push(b"event: response.output_text.del")
            .unwrap()
            .is_empty()
    );
    assert!(
        decoder
            .push(b"ta\ndata: {\"delta\":\"hi\"}\n")
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        decoder.push(b"\n").unwrap(),
        [SseFrame {
            event: Some("response.output_text.delta".to_owned()),
            data: "{\"delta\":\"hi\"}".to_owned(),
        }]
    );
}

#[test]
fn decoder_enforces_the_configured_frame_limit_and_fails_closed() {
    let mut decoder = SseDecoder::new(8);

    assert!(decoder.push(b"data: x\n").unwrap().is_empty());
    assert_eq!(
        decoder.push(b"\n").unwrap(),
        [SseFrame {
            event: None,
            data: "x".to_owned(),
        }]
    );

    assert!(decoder.push(b"data: xx").unwrap().is_empty());
    assert_eq!(
        decoder.push(b"\n").unwrap_err(),
        ProtocolError::FrameTooLarge { limit: 8 }
    );
    assert_eq!(
        decoder.push(b"data: safe\n\n").unwrap_err(),
        ProtocolError::DecoderFailed
    );
}

#[test]
fn decoder_defaults_to_a_strict_one_mibibyte_limit() {
    assert_eq!(DEFAULT_MAX_SSE_FRAME_BYTES, 1024 * 1024);
    let mut decoder = SseDecoder::default();
    let oversized = vec![b'x'; DEFAULT_MAX_SSE_FRAME_BYTES + 1];

    assert_eq!(
        decoder.push(&oversized).unwrap_err(),
        ProtocolError::FrameTooLarge {
            limit: DEFAULT_MAX_SSE_FRAME_BYTES,
        }
    );
}

#[test]
fn decoder_bounds_frames_returned_by_one_push_and_fails_closed() {
    let limit = 4096;
    let mut at_limit = SseDecoder::new(1024);
    assert_eq!(
        at_limit.push(&repeated_sse_frames(limit)).unwrap().len(),
        limit
    );

    let mut above_limit = SseDecoder::new(1024);
    let error = above_limit
        .push(&repeated_sse_frames(limit + 1))
        .expect_err("one push must not aggregate an unbounded frame vector");
    assert_eq!(format!("{error:?}"), "TooManyFrames { limit: 4096 }");
    assert_eq!(
        above_limit.push(b"data: later\n\n"),
        Err(ProtocolError::DecoderFailed)
    );
}

#[test]
fn decoder_supports_a_custom_per_push_frame_limit() {
    let mut at_limit = SseDecoder::with_limits(1024, 2);
    assert_eq!(at_limit.push(&repeated_sse_frames(2)).unwrap().len(), 2);

    let mut above_limit = SseDecoder::with_limits(1024, 2);
    assert_eq!(
        above_limit.push(&repeated_sse_frames(3)),
        Err(ProtocolError::TooManyFrames { limit: 2 })
    );
    assert_eq!(
        above_limit.push(b"data: later\n\n"),
        Err(ProtocolError::DecoderFailed)
    );
}

#[test]
fn decoder_rejects_invalid_utf8_and_fails_closed() {
    let mut decoder = SseDecoder::new(1024);

    assert_eq!(
        decoder.push(b"data: \xff\n").unwrap_err(),
        ProtocolError::InvalidUtf8
    );
    assert_eq!(
        decoder.push(b"data: valid\n\n").unwrap_err(),
        ProtocolError::DecoderFailed
    );
}

#[test]
fn decoder_finish_rejects_an_unterminated_tail_and_is_permanent() {
    let mut truncated = SseDecoder::new(1024);
    assert!(truncated.push(b"data: {\"partial\":").unwrap().is_empty());
    assert_eq!(
        truncated.finish().unwrap_err(),
        ProtocolError::UnexpectedEof
    );
    assert_eq!(
        truncated.push(b"true}\n\n").unwrap_err(),
        ProtocolError::DecoderFailed
    );
    assert_eq!(
        truncated.finish().unwrap_err(),
        ProtocolError::DecoderFailed
    );

    let mut complete = SseDecoder::new(1024);
    assert_eq!(
        complete.push(b"data: {}\n\n").unwrap(),
        [SseFrame {
            event: None,
            data: "{}".to_owned(),
        }]
    );
    assert!(complete.finish().unwrap().is_empty());
    assert_eq!(complete.finish().unwrap_err(), ProtocolError::DecoderFailed);
    assert_eq!(
        complete.push(b"data: {}\n\n").unwrap_err(),
        ProtocolError::DecoderFailed
    );
}

#[test]
fn encoder_writes_compact_json_and_a_standard_frame_terminator() {
    let encoded = encode_sse(
        Some("response.output_text.delta"),
        &json!({"line": "one\ntwo", "delta": "hi"}),
    );

    assert_eq!(
        encoded.as_ref(),
        b"event: response.output_text.delta\ndata: {\"delta\":\"hi\",\"line\":\"one\\ntwo\"}\n\n"
    );
}

#[test]
fn encoder_omits_an_event_name_that_could_inject_another_frame() {
    let encoded = encode_sse(Some("safe\n\ndata: injected\r"), &json!({"ok": true}));

    assert_eq!(encoded.as_ref(), b"data: {\"ok\":true}\n\n");
    assert_eq!(
        decode_in_chunks(&encoded, vec![1; encoded.len()]),
        [SseFrame {
            event: None,
            data: "{\"ok\":true}".to_owned(),
        }]
    );
}

#[test]
fn bounded_channel_rejects_zero_capacity() {
    assert!(matches!(
        bounded_event_channel::<u8>(0),
        Err(ProtocolError::InvalidChannelCapacity)
    ));
}

#[test]
fn bounded_channel_capacity_respects_the_tokio_semaphore_limit() {
    for capacity in [Semaphore::MAX_PERMITS - 1, Semaphore::MAX_PERMITS] {
        assert!(bounded_event_channel::<u8>(capacity).is_ok());
    }
    for capacity in [Semaphore::MAX_PERMITS + 1, usize::MAX] {
        let error = match bounded_event_channel::<u8>(capacity) {
            Err(error) => error,
            Ok(_) => panic!("capacity above Tokio's maximum must be rejected"),
        };
        assert_eq!(error, ProtocolError::InvalidChannelCapacity);
        assert_eq!(
            error.to_string(),
            "event channel capacity must be between 1 and the runtime-supported maximum"
        );
    }
}

#[tokio::test]
async fn bounded_channel_applies_real_backpressure() {
    let (sender, mut receiver) = bounded_event_channel(1).unwrap();
    sender.send(1_u8).await.unwrap();
    let mut blocked_send = tokio::spawn({
        let sender = sender.clone();
        async move { sender.send(2_u8).await }
    });

    assert!(
        tokio::time::timeout(Duration::from_millis(50), &mut blocked_send)
            .await
            .is_err()
    );
    assert_eq!(receiver.recv().await, Some(1));
    tokio::time::timeout(Duration::from_secs(1), &mut blocked_send)
        .await
        .expect("sender remained blocked after capacity was released")
        .unwrap()
        .unwrap();
    assert_eq!(receiver.recv().await, Some(2));
}

#[tokio::test]
async fn cancelling_a_receive_wakes_the_waiter() {
    let (_sender, mut receiver) = bounded_event_channel::<u8>(1).unwrap();
    let (cancel_sender, cancel_receiver) = tokio::sync::oneshot::channel();
    let waiting = tokio::spawn(async move {
        receiver
            .recv_or_cancel(async {
                let _ = cancel_receiver.await;
            })
            .await
    });

    cancel_sender.send(()).unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), waiting)
            .await
            .expect("cancelled receiver did not wake")
            .unwrap(),
        Err(ReceiveError::Cancelled)
    );
}

#[tokio::test]
async fn dropping_receiver_releases_a_blocked_sender() {
    let (sender, receiver) = bounded_event_channel(1).unwrap();
    sender.send(1_u8).await.unwrap();
    let blocked_send = tokio::spawn(async move { sender.send(2_u8).await });

    drop(receiver);
    assert!(
        tokio::time::timeout(Duration::from_secs(1), blocked_send)
            .await
            .expect("sender remained blocked after receiver was dropped")
            .unwrap()
            .is_err()
    );
}

#[tokio::test]
async fn dropping_all_senders_makes_receiver_report_eof() {
    let (sender, mut receiver) = bounded_event_channel::<u8>(1).unwrap();
    drop(sender);

    assert_eq!(
        tokio::time::timeout(Duration::from_secs(1), receiver.recv_or_cancel(pending()))
            .await
            .expect("receiver did not observe sender EOF"),
        Ok(None)
    );
}
