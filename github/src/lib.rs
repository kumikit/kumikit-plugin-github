//! `kumikit.github`: GitHub REST API tools over `wasi:http` (`api.github.com`), with a
//! fine-grained personal access token from Kumikit's secret store (`github-token`).
//!
//! Built solely with the public `kumikit-tool` SDK (`kumikit:tool/plugin@0.1.0`).

pub mod api;

use kumikit_tool::{Error, ErrorCode, Plugin, Tool, ToolCall, ToolResult, http, secrets};
use serde_json::{Value, json};

struct GitHub;

impl Plugin for GitHub {
    fn activate() -> Result<(), Error> {
        Ok(())
    }

    fn deactivate() {}
}

impl Tool for GitHub {
    fn call(call: ToolCall) -> Result<ToolResult, Error> {
        let input: Value = serde_json::from_str(&call.input).unwrap_or_else(|_| json!({}));
        let token = match secrets::get("github-token") {
            Ok(v) => v,
            Err(e) => {
                return Ok(ToolResult::failed(format!(
                    "could not read github-token: {}",
                    e.message
                )));
            }
        };
        if token.is_none()
            && matches!(
                call.name.as_str(),
                "create_pull_request" | "create_issue_comment"
            )
        {
            return Ok(ToolResult::failed(
                "github-token is not set; configure it in Settings -> GitHub",
            ));
        }
        let req = match api::build_request(&call.name, &input, token.as_deref()) {
            Ok(r) => r,
            Err(e) if e.starts_with("unknown tool:") => {
                return Err(Error::new(ErrorCode::NotFound, e));
            }
            Err(e) => return Ok(ToolResult::failed(e)),
        };
        let resp = match http::send(&req) {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolResult::failed(format!(
                    "GitHub request failed: {}",
                    e.message
                )));
            }
        };
        match api::parse_response(&call.name, &resp) {
            Ok((text, structured)) => Ok(ToolResult {
                text,
                structured: structured.map(|v| v.to_string()),
                is_error: false,
            }),
            Err(e) => Ok(ToolResult::failed(e)),
        }
    }
}

kumikit_tool::export_tool!(GitHub);
