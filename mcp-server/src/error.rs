//! MCP error type that maps HTTP status codes from the backend to MCP error
//! codes (spec §6). The [`McpError`] wraps [`rmcp::ErrorData`] so tool handlers
//! can return `Result<_, McpError>` and the error flows through as a clean
//! JSON-RPC error to the agent.

use rmcp::model::ErrorData;

/// An error from a tool call, carrying an MCP error code + human-readable message.
pub struct McpError {
    code: i32,
    message: String,
}

impl McpError {
    pub fn internal(msg: impl Into<String>) -> Self {
        Self {
            code: -32603,
            message: msg.into(),
        }
    }

    /// Bad tool input caught before any HTTP call (spec §6): invalid base64,
    /// both/neither owner id, payload over the size limit.
    pub fn invalid_params(msg: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: msg.into(),
        }
    }

    /// Maps an HTTP status from the backend to the appropriate MCP error code.
    /// Tries to extract the backend's JSON error message; falls back to the
    /// HTTP status reason.
    pub fn from_http(status: reqwest::StatusCode, body: String) -> Self {
        // Try to parse the backend's error JSON: { "error": "...", "message": "..." }
        let backend_msg = serde_json::from_str::<serde_json::Value>(&body)
            .ok()
            .and_then(|v| v.get("message").and_then(|m| m.as_str()).map(String::from));

        let (code, default_msg) = match status.as_u16() {
            400 => (-32602, "Invalid request parameters"),
            401 => (-32002, "Agent token is invalid or revoked"),
            403 => (-32001, "Forbidden: insufficient permissions"),
            404 => (-32004, "Not found"),
            409 => (-32006, "Conflict"),
            429 => (-32008, "Too many requests"),
            _ if status.is_server_error() => (-32603, "Internal server error"),
            _ => (-32603, "Unexpected error from backend"),
        };
        Self {
            code,
            message: backend_msg.unwrap_or_else(|| default_msg.to_string()),
        }
    }
}

impl From<reqwest::Error> for McpError {
    fn from(e: reqwest::Error) -> Self {
        Self::internal(format!("HTTP request failed: {e}"))
    }
}

impl From<McpError> for ErrorData {
    fn from(e: McpError) -> Self {
        ErrorData::new(rmcp::model::ErrorCode(e.code), e.message, None)
    }
}

impl std::fmt::Display for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.code, self.message)
    }
}

impl std::fmt::Debug for McpError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "McpError {{ code: {}, message: {:?} }}", self.code, self.message)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_http_maps_statuses_to_mcp_codes() {
        let cases = [
            (400, -32602),
            (401, -32002),
            (403, -32001),
            (404, -32004),
            (409, -32006),
            (429, -32008),
            (500, -32603),
        ];
        for (status, code) in cases {
            let e = McpError::from_http(reqwest::StatusCode::from_u16(status).unwrap(), String::new());
            assert_eq!(e.code, code, "HTTP {status}");
        }
    }

    #[test]
    fn from_http_prefers_the_backends_message() {
        let body = r#"{"error":"Forbidden","message":"Task 42 is not assigned to you"}"#;
        let e = McpError::from_http(reqwest::StatusCode::FORBIDDEN, body.to_string());
        assert_eq!(e.code, -32001);
        assert_eq!(e.message, "Task 42 is not assigned to you");
    }

    #[test]
    fn invalid_params_is_json_rpc_invalid_params() {
        let e = McpError::invalid_params("exactly one of taskId/featureId must be set");
        assert_eq!(e.code, -32602);
        assert!(e.message.contains("taskId"));
    }
}
