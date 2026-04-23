use std::io::{self, BufRead, Write};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const JSONRPC_VERSION: &str = "2.0";
const MCP_PROTOCOL_VERSION: &str = "2025-11-25";
const SERVER_NAME: &str = "oxide-mcp";

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const SERVER_NOT_READY: i64 = -32002;
const RESOURCE_NOT_FOUND: i64 = -32010;
const PROMPT_NOT_FOUND: i64 = -32011;
const TOOL_NOT_FOUND: i64 = -32012;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(untagged)]
enum RequestId {
    Integer(i64),
    String(String),
    Null,
}

#[derive(Debug, Deserialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: RequestId,
    method: String,
    #[serde(default)]
    params: Value,
}

#[derive(Debug, Deserialize)]
struct JsonRpcNotification {
    jsonrpc: String,
    method: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    data: Option<Value>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InitializeParams {
    protocol_version: String,
    #[serde(default)]
    capabilities: Value,
    #[serde(default)]
    client_info: Option<ImplementationInfo>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ImplementationInfo {
    name: String,
    version: String,
}

#[derive(Debug, Deserialize)]
struct ToolCallParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Deserialize)]
struct ResourceReadParams {
    uri: String,
}

#[derive(Debug, Deserialize)]
struct PromptGetParams {
    name: String,
    #[serde(default)]
    arguments: Value,
}

#[derive(Debug, Deserialize)]
struct LoggingSetLevelParams {
    level: String,
}

#[derive(Debug, Deserialize)]
struct CompletionCompleteParams {
    #[serde(default)]
    r#ref: Value,
    #[serde(default)]
    argument: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum InitState {
    #[default]
    Uninitialized,
    Initializing,
    Initialized,
}

#[derive(Debug, Default)]
struct SessionState {
    init_state: InitState,
    client_info: Option<ImplementationInfo>,
    client_capabilities: Value,
    log_level: String,
}

fn main() {
    if let Err(error) = run_stdio_server() {
        let _ = writeln!(io::stderr(), "server error: {error}");
    }
}

fn run_stdio_server() -> io::Result<()> {
    let stdin = io::stdin();
    let stdout = io::stdout();
    let mut reader = io::BufReader::new(stdin.lock());
    let mut writer = io::BufWriter::new(stdout.lock());
    let mut state = SessionState::default();
    let mut line = String::new();

    loop {
        line.clear();
        let bytes = reader.read_line(&mut line)?;
        if bytes == 0 {
            break;
        }

        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }

        let parsed: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(error) => {
                let response = error_response(
                    RequestId::Null,
                    PARSE_ERROR,
                    "Parse error",
                    Some(json!({ "details": error.to_string() })),
                );
                write_response(&mut writer, &response)?;
                continue;
            }
        };

        if let Some(response) = dispatch_message(parsed, &mut state) {
            write_response(&mut writer, &response)?;
        }
    }

    writer.flush()?;
    Ok(())
}

fn write_response(writer: &mut impl Write, value: &Value) -> io::Result<()> {
    serde_json::to_writer(&mut *writer, value)?;
    writer.write_all(b"\n")?;
    writer.flush()
}

fn dispatch_message(value: Value, state: &mut SessionState) -> Option<Value> {
    match value {
        Value::Array(values) => {
            if values.is_empty() {
                return Some(error_response(
                    RequestId::Null,
                    INVALID_REQUEST,
                    "Invalid Request",
                    None,
                ));
            }
            let mut responses = Vec::new();
            for item in values {
                if let Some(response) = dispatch_single(item, state) {
                    responses.push(response);
                }
            }
            if responses.is_empty() {
                None
            } else {
                Some(Value::Array(responses))
            }
        }
        _ => dispatch_single(value, state),
    }
}

fn dispatch_single(value: Value, state: &mut SessionState) -> Option<Value> {
    if value.get("method").is_none() {
        return Some(error_response(
            RequestId::Null,
            INVALID_REQUEST,
            "Invalid Request",
            None,
        ));
    }

    if value.get("id").is_some() {
        let request: JsonRpcRequest = match serde_json::from_value(value) {
            Ok(request) => request,
            Err(error) => {
                return Some(error_response(
                    RequestId::Null,
                    INVALID_REQUEST,
                    "Invalid Request",
                    Some(json!({ "details": error.to_string() })),
                ));
            }
        };
        Some(handle_request(request, state))
    } else {
        let notification: JsonRpcNotification = match serde_json::from_value(value) {
            Ok(notification) => notification,
            Err(_) => return None,
        };
        handle_notification(notification, state);
        None
    }
}

fn handle_notification(notification: JsonRpcNotification, state: &mut SessionState) {
    if notification.jsonrpc != JSONRPC_VERSION {
        return;
    }
    if notification.method == "notifications/initialized"
        && state.init_state == InitState::Initializing
    {
        state.init_state = InitState::Initialized;
    }
}

fn handle_request(request: JsonRpcRequest, state: &mut SessionState) -> Value {
    if request.jsonrpc != JSONRPC_VERSION {
        return error_response(
            request.id,
            INVALID_REQUEST,
            "Invalid jsonrpc version",
            Some(json!({ "expected": JSONRPC_VERSION })),
        );
    }

    if let Err(error) = ensure_request_allowed(state, &request.method) {
        return error_response(request.id, error.code, &error.message, error.data);
    }

    match request.method.as_str() {
        "initialize" => handle_initialize(request.id, request.params, state),
        "ping" => success_response(request.id, json!({})),
        "tools/list" => success_response(request.id, json!({ "tools": [] })),
        "tools/call" => handle_tools_call(request.id, request.params),
        "resources/list" => success_response(request.id, json!({ "resources": [] })),
        "resources/templates/list" => {
            success_response(request.id, json!({ "resourceTemplates": [] }))
        }
        "resources/read" => handle_resources_read(request.id, request.params),
        "prompts/list" => success_response(request.id, json!({ "prompts": [] })),
        "prompts/get" => handle_prompts_get(request.id, request.params),
        "completion/complete" => handle_completion_complete(request.id, request.params),
        "logging/setLevel" => handle_logging_set_level(request.id, request.params, state),
        _ => error_response(request.id, METHOD_NOT_FOUND, "Method not found", None),
    }
}

fn ensure_request_allowed(state: &SessionState, method: &str) -> Result<(), JsonRpcError> {
    if method == "initialize" || method == "ping" {
        return Ok(());
    }

    match state.init_state {
        InitState::Uninitialized => Err(JsonRpcError {
            code: SERVER_NOT_READY,
            message: "Server not initialized".to_string(),
            data: None,
        }),
        InitState::Initializing => Err(JsonRpcError {
            code: SERVER_NOT_READY,
            message: "Waiting for notifications/initialized".to_string(),
            data: None,
        }),
        InitState::Initialized => Ok(()),
    }
}

fn handle_initialize(id: RequestId, params: Value, state: &mut SessionState) -> Value {
    if state.init_state != InitState::Uninitialized {
        return error_response(
            id,
            INVALID_REQUEST,
            "Initialize can only be called once per session",
            None,
        );
    }

    let params: InitializeParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return error_response(
                id,
                INVALID_PARAMS,
                "Invalid initialize params",
                Some(json!({ "details": error.to_string() })),
            );
        }
    };

    if params.protocol_version != MCP_PROTOCOL_VERSION {
        return error_response(
            id,
            INVALID_PARAMS,
            "Unsupported protocol version",
            Some(json!({ "supported": [MCP_PROTOCOL_VERSION] })),
        );
    }

    state.init_state = InitState::Initializing;
    state.client_info = params.client_info;
    state.client_capabilities = params.capabilities;

    success_response(
        id,
        json!({
            "protocolVersion": MCP_PROTOCOL_VERSION,
            "capabilities": {
                "completion": {},
                "logging": {},
                "prompts": { "listChanged": false },
                "resources": { "listChanged": false, "subscribe": false },
                "tools": { "listChanged": false }
            },
            "serverInfo": {
                "name": SERVER_NAME,
                "version": env!("CARGO_PKG_VERSION")
            },
            "instructions": "MCP server implementation with full protocol method coverage and strict lifecycle handling."
        }),
    )
}

fn handle_tools_call(id: RequestId, params: Value) -> Value {
    let params: ToolCallParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return error_response(
                id,
                INVALID_PARAMS,
                "Invalid tools/call params",
                Some(json!({ "details": error.to_string() })),
            );
        }
    };

    let _ = params.arguments;
    error_response(
        id,
        TOOL_NOT_FOUND,
        "Tool not found",
        Some(json!({
            "toolName": params.name
        })),
    )
}

fn handle_resources_read(id: RequestId, params: Value) -> Value {
    let params: ResourceReadParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return error_response(
                id,
                INVALID_PARAMS,
                "Invalid resources/read params",
                Some(json!({ "details": error.to_string() })),
            );
        }
    };

    error_response(
        id,
        RESOURCE_NOT_FOUND,
        "Resource not found",
        Some(json!({ "uri": params.uri })),
    )
}

fn handle_prompts_get(id: RequestId, params: Value) -> Value {
    let params: PromptGetParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return error_response(
                id,
                INVALID_PARAMS,
                "Invalid prompts/get params",
                Some(json!({ "details": error.to_string() })),
            );
        }
    };

    let _ = params.arguments;
    error_response(
        id,
        PROMPT_NOT_FOUND,
        "Prompt not found",
        Some(json!({ "name": params.name })),
    )
}

fn handle_completion_complete(id: RequestId, params: Value) -> Value {
    let params: CompletionCompleteParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return error_response(
                id,
                INVALID_PARAMS,
                "Invalid completion/complete params",
                Some(json!({ "details": error.to_string() })),
            );
        }
    };

    let _ = params.r#ref;
    let _ = params.argument;
    success_response(
        id,
        json!({
            "completion": {
                "values": [],
                "hasMore": false
            }
        }),
    )
}

fn handle_logging_set_level(id: RequestId, params: Value, state: &mut SessionState) -> Value {
    let params: LoggingSetLevelParams = match serde_json::from_value(params) {
        Ok(params) => params,
        Err(error) => {
            return error_response(
                id,
                INVALID_PARAMS,
                "Invalid logging/setLevel params",
                Some(json!({ "details": error.to_string() })),
            );
        }
    };

    state.log_level = params.level;
    success_response(id, json!({}))
}

fn success_response(id: RequestId, result: Value) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "result": result
    })
}

fn error_response(id: RequestId, code: i64, message: &str, data: Option<Value>) -> Value {
    json!({
        "jsonrpc": JSONRPC_VERSION,
        "id": id,
        "error": {
            "code": code,
            "message": message,
            "data": data
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init_request() -> Value {
        json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {
                "protocolVersion": "2025-11-25",
                "capabilities": {},
                "clientInfo": { "name": "test-client", "version": "1.0.0" }
            }
        })
    }

    #[test]
    fn initialize_then_initialized_allows_feature_methods() {
        let mut state = SessionState::default();
        let response = dispatch_message(init_request(), &mut state).expect("response");
        assert_eq!(response["result"]["protocolVersion"], "2025-11-25");
        assert_eq!(state.init_state, InitState::Initializing);

        let notification = json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized",
            "params": {}
        });
        assert!(dispatch_message(notification, &mut state).is_none());
        assert_eq!(state.init_state, InitState::Initialized);

        let tools_list = json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "tools/list",
            "params": {}
        });
        let tools_response = dispatch_message(tools_list, &mut state).expect("tools response");
        assert!(tools_response["result"]["tools"].is_array());
    }

    #[test]
    fn method_rejected_before_initialization() {
        let mut state = SessionState::default();
        let request = json!({
            "jsonrpc": "2.0",
            "id": 9,
            "method": "prompts/list",
            "params": {}
        });
        let response = dispatch_message(request, &mut state).expect("response");
        assert_eq!(response["error"]["code"], SERVER_NOT_READY);
    }

    #[test]
    fn tools_call_unknown_returns_mcp_error() {
        let mut state = SessionState {
            init_state: InitState::Initialized,
            ..SessionState::default()
        };
        let request = json!({
            "jsonrpc": "2.0",
            "id": "call-1",
            "method": "tools/call",
            "params": {
                "name": "unknown_tool",
                "arguments": {}
            }
        });
        let response = dispatch_message(request, &mut state).expect("response");
        assert_eq!(response["error"]["code"], TOOL_NOT_FOUND);
    }

    #[test]
    fn empty_batch_is_invalid_request() {
        let mut state = SessionState::default();
        let response = dispatch_message(json!([]), &mut state).expect("response");
        assert_eq!(response["error"]["code"], INVALID_REQUEST);
    }

    #[test]
    fn batch_mixes_notifications_and_requests() {
        let mut state = SessionState {
            init_state: InitState::Initialized,
            ..SessionState::default()
        };
        let batch = json!([
            {
                "jsonrpc": "2.0",
                "method": "notifications/initialized",
                "params": {}
            },
            {
                "jsonrpc": "2.0",
                "id": 3,
                "method": "ping",
                "params": {}
            }
        ]);
        let response = dispatch_message(batch, &mut state).expect("batch response");
        assert!(response.is_array());
        assert_eq!(response[0]["id"], 3);
        assert!(response[0]["result"].is_object());
    }
}
