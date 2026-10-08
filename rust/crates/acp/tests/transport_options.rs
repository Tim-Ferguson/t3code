use serde_json::Value;
use t3_acp::transport::{NdjsonDecoder, decoded_message, outgoing_decoded, outgoing_wire};

#[test]
fn strict_chunk_decoder_and_outgoing_bytes_match_original_parser_and_serializer() {
    for line in include_str!("fixtures/transport.jsonl").lines() {
        let case: Value = serde_json::from_str(line).unwrap();
        if case["kind"] == "decode" {
            let mut decoder = NdjsonDecoder::new(16 * 1024 * 1024);
            for (chunk, observation) in case["chunks"]
                .as_array()
                .unwrap()
                .iter()
                .zip(case["observations"].as_array().unwrap())
            {
                let bytes: Vec<u8> = chunk
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|byte| byte.as_u64().unwrap() as u8)
                    .collect();
                let actual = decoder.decode(&bytes);
                if observation["success"] == true {
                    let values = actual.unwrap_or_else(|error| panic!("{}: {error}", case["name"]));
                    assert_eq!(
                        serde_json::to_value(
                            values.iter().map(decoded_message).collect::<Vec<_>>()
                        )
                        .unwrap(),
                        observation["messages"],
                        "{}",
                        case["name"]
                    );
                } else {
                    assert!(actual.is_err(), "{}", case["name"]);
                }
            }
        } else {
            let wire = outgoing_wire(&case["wire"]);
            assert_eq!(
                serde_json::to_string(&wire).unwrap() + "\n",
                case["raw"].as_str().unwrap()
            );
            assert_eq!(outgoing_decoded(&wire), case["message"]);
            assert!(wire.get("headers").is_none());
        }
    }
}

#[tokio::test]
async fn enabled_logging_without_custom_logger_emits_structured_debug_and_respects_direction_flags()
{
    use std::{
        io::Write,
        sync::{Arc, Mutex},
    };
    use t3_acp::transport::{LogDirection, LogStage, ProtocolLogEvent, ProtocolOptions};
    use tracing::instrument::WithSubscriber;
    #[derive(Clone)]
    struct Buffer(Arc<Mutex<Vec<u8>>>);
    impl Write for Buffer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let captured = bytes.clone();
    let subscriber = tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .without_time()
        .with_ansi(false)
        .with_writer(move || Buffer(captured.clone()))
        .finish();
    let options = ProtocolOptions {
        log_incoming: true,
        ..Default::default()
    };
    async {
        options
            .log(ProtocolLogEvent {
                direction: LogDirection::Incoming,
                stage: LogStage::DecodeFailed,
                payload: serde_json::json!({"operation":"decode-wire-message"}),
            })
            .await;
        options
            .log(ProtocolLogEvent {
                direction: LogDirection::Outgoing,
                stage: LogStage::Raw,
                payload: serde_json::json!("disabled-private-wire"),
            })
            .await;
    }
    .with_subscriber(subscriber)
    .await;
    let output = String::from_utf8(bytes.lock().unwrap().clone()).unwrap();
    assert!(output.contains("ACP protocol event"));
    assert!(output.contains("event="));
    assert!(output.contains("Incoming"));
    assert!(output.contains("decode-wire-message"));
    assert!(!output.contains("disabled-private-wire"));
}
