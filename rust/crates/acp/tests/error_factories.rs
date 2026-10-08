use serde::Deserialize;
use serde_json::{Value, json};
use std::{io::Read, sync::Arc};
use t3_acp::{AcpError, RequestId, RpcError, errors::*};

#[derive(Deserialize)]
struct Case {
    operation: String,
    input: Value,
    output: Value,
}
fn text(input: &Value, key: &str) -> Option<String> {
    input.get(key).and_then(Value::as_str).map(str::to_owned)
}
fn cause() -> FailureCause {
    json!({"secret":"private rejected payload"}).into()
}
fn transport() -> TransportError {
    TransportError {
        operation: None,
        method: None,
        detail: None,
        pid: None,
        cause: cause(),
    }
}
fn request_id(input: &Value) -> RequestId {
    serde_json::from_value(input["requestId"].clone()).unwrap()
}
fn parse() -> ProtocolParseError {
    ProtocolParseError::from_encoding_error(None, None, cause())
}
fn observe(error: Failure) -> Value {
    let (tag, diagnostics, code, protocol, has_cause) = match &error {
        Failure::Request(error) => (
            "AcpRequestError",
            Some(&error.diagnostics),
            Some(error.code),
            Some(error.to_protocol_error()),
            error.diagnostics.cause.is_some(),
        ),
        Failure::Spawn(_) => ("AcpSpawnError", None, None, None, true),
        Failure::ProcessExited(e) => (
            "AcpProcessExitedError",
            None,
            e.code.as_ref().and_then(|v| v.as_i64()),
            None,
            e.cause.is_some(),
        ),
        Failure::ProtocolParse(_) => ("AcpProtocolParseError", None, None, None, true),
        Failure::Transport(_) => ("AcpTransportError", None, None, None, true),
        Failure::InputStreamEnded(_) => ("AcpInputStreamEndedError", None, None, None, false),
    };
    let mut value = json!({"tag":tag,"message":error.to_string(),"hasCause":has_cause});
    if let Some(code) = code {
        value["code"] = json!(code);
    }
    if let Failure::ProcessExited(e) = &error {
        if let Some(code) = &e.code {
            value["code"] = json!(code);
        }
    }
    if let Some(protocol) = protocol {
        value["protocol"] = serde_json::to_value(protocol).unwrap();
    }
    if let Some(d) = diagnostics {
        if let Some(method) = &d.method {
            value["method"] = json!(method);
        }
        if let Some(id) = &d.request_id {
            value["requestId"] = json!(id);
        }
        if let Some(operation) = d.operation {
            value["operation"] = json!(operation);
        }
        if let Some(issues) = &d.issues {
            for (key, v) in serde_json::to_value(issues).unwrap().as_object().unwrap() {
                value[key] = v.clone();
            }
        }
        if let Some(FailureCause::Error(cause)) = &d.cause {
            if let AcpError::Failure(cause) = cause.as_ref() {
                value["causeTag"] = observe(cause.as_ref().clone())["tag"].clone();
            }
        }
    }
    match &error {
        Failure::ProtocolParse(e) => {
            value["operation"] = json!(e.operation);
            if let Some(method) = &e.method {
                value["method"] = json!(method);
            }
            if let Some(id) = &e.request_id {
                value["requestId"] = json!(id);
            }
        }
        Failure::Transport(e) => {
            if let Some(operation) = e.operation {
                value["operation"] = json!(operation);
            }
            if let Some(method) = &e.method {
                value["method"] = json!(method);
            }
        }
        _ => {}
    }
    value
}
#[test]
fn original_error_factories_preserve_messages_context_and_public_wire_fields() {
    let mut source = String::new();
    flate2::read::GzDecoder::new(include_bytes!("fixtures/error-factories.jsonl.gz").as_slice())
        .read_to_string(&mut source)
        .unwrap();
    let mut count = 0;
    for line in source.lines() {
        let c: Case = serde_json::from_str(line).unwrap();
        let i = &c.input;
        let method = text(i, "method").unwrap_or_default();
        let message = text(i, "message");
        let data = i.get("data").cloned();
        let error = match c.operation.as_str() {
            "spawn" => Failure::Spawn(SpawnError {
                command: text(i, "command"),
                cause: cause(),
            }),
            "exit" => Failure::ProcessExited(ProcessExitedError {
                code: i.get("code").and_then(Value::as_number).cloned(),
                pid: Some(7),
                stderr: text(i, "stderr"),
                cause: Some(cause()),
            }),
            "parse" => Failure::ProtocolParse(ProtocolParseError {
                operation: serde_json::from_value(i["operation"].clone()).unwrap(),
                method: text(i, "method"),
                request_id: Some(RequestId::String("0".into())),
                issues: None,
                cause: cause(),
            }),
            "transport" => Failure::Transport(TransportError {
                operation: i
                    .get("operation")
                    .map(|v| serde_json::from_value(v.clone()).unwrap()),
                method: text(i, "method"),
                detail: Some("private transport detail".into()),
                pid: Some(7),
                cause: cause(),
            }),
            "input-end" => Failure::InputStreamEnded(InputStreamEndedError),
            operation => {
                let error = match operation {
                    "parseError" => RequestError::parse_error(message.as_deref(), data),
                    "invalidRequest" => RequestError::invalid_request(message.as_deref(), data),
                    "invalidParams" => RequestError::invalid_params(message.as_deref(), data),
                    "internalError" => {
                        RequestError::internal_error(message.as_deref(), data, Default::default())
                    }
                    "authRequired" => RequestError::auth_required(message.as_deref(), data),
                    "resourceNotFound" => {
                        RequestError::resource_not_found(message.as_deref(), data)
                    }
                    "methodNotFound" => RequestError::method_not_found(&method),
                    "fromProtocolError" => RequestError::from_protocol_error(
                        RpcError {
                            code: -32002,
                            message: "remote message".into(),
                            data,
                        },
                        &method,
                        Some(request_id(i)),
                        None,
                    ),
                    "fromExtensionResponseFailure" => {
                        RequestError::from_extension_response_failure(
                            &method,
                            request_id(i),
                            cause(),
                        )
                    }
                    "fromExtensionResponseEncodingError" => {
                        RequestError::from_extension_response_encoding_error(
                            &method,
                            request_id(i),
                            parse(),
                        )
                    }
                    "unsupportedStreamingResponse" => {
                        RequestError::unsupported_streaming_response(&method, request_id(i))
                    }
                    "fromCoreHandlerError" | "fromExtensionHandlerError" => {
                        let error = if i["request"] == true {
                            RequestError::auth_required(Some("custom auth"), Some(Value::Null))
                                .into()
                        } else {
                            transport().into()
                        };
                        if operation == "fromCoreHandlerError" {
                            RequestError::from_core_handler_error(error, &method)
                        } else {
                            RequestError::from_extension_handler_error(error, &method)
                        }
                    }
                    other => panic!("unhandled {other}"),
                };
                Failure::Request(Box::new(error))
            }
        };
        assert_eq!(
            observe(error),
            c.output,
            "case {count}: {} {}",
            c.operation,
            c.input
        );
        count += 1;
    }
    assert_eq!(count, 88);
}
#[test]
fn schema_failure_retains_typed_cause_without_leaking_rejected_values() {
    let rejected =
        json!({"sessionId":"s","prompt":[{"type":"text","text":{"password":"private"}}]});
    let schema = t3_acp::v2::PromptRequest::decode(rejected.clone()).unwrap_err();
    let expected = schema.issue.diagnostics();
    let error = RequestError::invalid_extension_payload("x/prompt", schema.clone());
    assert_eq!(error.diagnostics.issues, Some(expected.clone()));
    assert_eq!(error.data, Some(serde_json::to_value(expected).unwrap()));
    let Some(FailureCause::Schema(cause)) = &error.diagnostics.cause else {
        panic!("lost schema cause")
    };
    assert_eq!(cause.cause, rejected);
    assert_eq!(cause.as_ref(), &schema);
    assert!(!error.to_string().contains("private"));
    assert!(
        !serde_json::to_string(&error.to_protocol_error())
            .unwrap()
            .contains("password")
    );
    let nested = RequestError::from_extension_handler_error(
        TransportError {
            operation: Some(TransportOperation::CallRpc),
            method: Some("x/private".into()),
            detail: Some("private".into()),
            pid: None,
            cause: cause.clone().as_ref().clone().into(),
        }
        .into(),
        "x/handler",
    );
    let Some(FailureCause::Error(cause)) = &nested.diagnostics.cause else {
        panic!("lost typed transport cause")
    };
    assert!(
        matches!(cause.as_ref(),AcpError::Failure(failure)if matches!(failure.as_ref(),Failure::Transport(_)))
    );
    assert_eq!(nested.to_protocol_error().data, None);
    let request = Arc::new(Failure::Request(Box::new(error)));
    let preserved = RequestError::from_core_handler_error(AcpError::Failure(request), "new method");
    assert_eq!(preserved.diagnostics.method.as_deref(), Some("x/prompt"));
}
