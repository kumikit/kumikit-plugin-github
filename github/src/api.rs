//! Pure GitHub REST API request building, response formatting, and rate-limit handling,
//! tested natively without I/O.

use std::time::Duration;

use kumikit_tool::http::{Request, Response};
use serde_json::{Value, json};

const API_BASE: &str = "https://api.github.com";
const TIMEOUT: Duration = Duration::from_secs(30);

/// Validates a GitHub owner or repository name segment (refuses path traversal and query
/// characters).
pub fn validate_segment<'a>(name: &str, value: &'a str) -> Result<&'a str, String> {
    if value.is_empty() || value == "." || value == ".." {
        return Err(format!("{name} is invalid"));
    }
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
    {
        Ok(value)
    } else {
        Err(format!("{name} contains invalid characters"))
    }
}

/// Validates a git ref (commit SHA, tag or branch name, allowing `/` inside branch names
/// while refusing `..`, leading/trailing `/`, query or fragment characters).
pub fn validate_ref(value: &str) -> Result<&str, String> {
    if value.is_empty()
        || value.starts_with('/')
        || value.ends_with('/')
        || value.contains("..")
        || value.contains("//")
    {
        return Err("ref is invalid".into());
    }
    if value
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '/'))
    {
        Ok(value)
    } else {
        Err("ref contains invalid characters".into())
    }
}

fn with_headers(mut req: Request, token: Option<&str>) -> Request {
    req = req
        .header("accept", "application/vnd.github+json")
        .header("x-github-api-version", "2022-11-28")
        .header(
            "user-agent",
            concat!("kumikit-github/", env!("CARGO_PKG_VERSION")),
        )
        .timeout(TIMEOUT);
    if let Some(t) = token
        && !t.is_empty()
    {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    req
}

fn req_str<'a>(input: &'a Value, key: &str) -> Result<&'a str, String> {
    input
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| format!("{key} is required"))
}

fn req_u64(input: &Value, key: &str) -> Result<u64, String> {
    input
        .get(key)
        .and_then(Value::as_u64)
        .filter(|&n| n >= 1)
        .ok_or_else(|| format!("{key} must be a positive integer"))
}

/// Builds the outgoing HTTPS request for one tool call.
pub fn build_request(tool: &str, input: &Value, token: Option<&str>) -> Result<Request, String> {
    let owner = validate_segment("owner", req_str(input, "owner")?)?;
    let repo = validate_segment("repo", req_str(input, "repo")?)?;
    let req = match tool {
        "pull_request_get" => {
            let n = req_u64(input, "number")?;
            Request::get(format!("{API_BASE}/repos/{owner}/{repo}/pulls/{n}"))
        }
        "pull_requests_list" => {
            let state = match input.get("state").and_then(Value::as_str) {
                Some("closed") => "closed",
                Some("all") => "all",
                _ => "open",
            };
            let per_page = input
                .get("per_page")
                .and_then(Value::as_u64)
                .unwrap_or(30)
                .clamp(1, 100);
            Request::get(format!(
                "{API_BASE}/repos/{owner}/{repo}/pulls?state={state}&per_page={per_page}"
            ))
        }
        "issue_get" => {
            let n = req_u64(input, "number")?;
            Request::get(format!("{API_BASE}/repos/{owner}/{repo}/issues/{n}"))
        }
        "issues_list" => {
            let state = match input.get("state").and_then(Value::as_str) {
                Some("closed") => "closed",
                Some("all") => "all",
                _ => "open",
            };
            let per_page = input
                .get("per_page")
                .and_then(Value::as_u64)
                .unwrap_or(30)
                .clamp(1, 100);
            Request::get(format!(
                "{API_BASE}/repos/{owner}/{repo}/issues?state={state}&per_page={per_page}"
            ))
        }
        "checks_for_ref" => {
            let r = validate_ref(req_str(input, "ref")?)?;
            Request::get(format!(
                "{API_BASE}/repos/{owner}/{repo}/commits/{r}/check-runs"
            ))
        }
        "create_pull_request" => {
            let title = req_str(input, "title")?;
            let head = req_str(input, "head")?;
            let base = req_str(input, "base")?;
            let mut body = json!({ "title": title, "head": head, "base": base });
            if let Some(b) = input.get("body").and_then(Value::as_str) {
                body["body"] = json!(b);
            }
            if let Some(d) = input.get("draft").and_then(Value::as_bool) {
                body["draft"] = json!(d);
            }
            Request::post(
                format!("{API_BASE}/repos/{owner}/{repo}/pulls"),
                "application/json",
                body.to_string().into_bytes(),
            )
        }
        "create_issue_comment" => {
            let n = req_u64(input, "number")?;
            let body_text = req_str(input, "body")?;
            let body = json!({ "body": body_text });
            Request::post(
                format!("{API_BASE}/repos/{owner}/{repo}/issues/{n}/comments"),
                "application/json",
                body.to_string().into_bytes(),
            )
        }
        other => return Err(format!("unknown tool: {other}")),
    };
    Ok(with_headers(req, token))
}

/// Interprets an HTTP response into `(summary_text, structured_json)` or a concise error
/// message (handling rate limits without retrying).
pub fn parse_response(tool: &str, resp: &Response) -> Result<(String, Option<Value>), String> {
    let body_json: Value = serde_json::from_slice(&resp.body).unwrap_or(Value::Null);
    if resp.status == 429
        || (resp.status == 403 && resp.header("x-ratelimit-remaining") == Some("0"))
    {
        if let Some(after) = resp.header("retry-after") {
            return Err(format!(
                "GitHub API rate limit exceeded; retry after {after} seconds"
            ));
        }
        if let Some(reset) = resp.header("x-ratelimit-reset") {
            return Err(format!(
                "GitHub API rate limit exceeded; resets at unix timestamp {reset}"
            ));
        }
        return Err("GitHub API rate limit exceeded; retry later".into());
    }
    if !(200..300).contains(&resp.status) {
        let msg = body_json["message"]
            .as_str()
            .filter(|s| !s.is_empty())
            .unwrap_or("request failed");
        return Err(format!("GitHub API HTTP {}: {msg}", resp.status));
    }

    let summary = match tool {
        "pull_request_get" | "create_pull_request" => {
            let n = body_json["number"].as_u64().unwrap_or(0);
            let title = body_json["title"].as_str().unwrap_or_default();
            let state = body_json["state"].as_str().unwrap_or_default();
            let url = body_json["html_url"].as_str().unwrap_or_default();
            format!("PR #{n} [{state}] {title}\n{url}")
        }
        "pull_requests_list" => {
            let items = body_json.as_array().map(Vec::as_slice).unwrap_or_default();
            if items.is_empty() {
                "No pull requests.".into()
            } else {
                items
                    .iter()
                    .map(|pr| {
                        format!(
                            "#{} [{}] {}",
                            pr["number"].as_u64().unwrap_or(0),
                            pr["state"].as_str().unwrap_or_default(),
                            pr["title"].as_str().unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        "issue_get" => {
            let n = body_json["number"].as_u64().unwrap_or(0);
            let title = body_json["title"].as_str().unwrap_or_default();
            let state = body_json["state"].as_str().unwrap_or_default();
            let url = body_json["html_url"].as_str().unwrap_or_default();
            format!("Issue #{n} [{state}] {title}\n{url}")
        }
        "issues_list" => {
            let items = body_json.as_array().map(Vec::as_slice).unwrap_or_default();
            if items.is_empty() {
                "No issues.".into()
            } else {
                items
                    .iter()
                    .map(|iss| {
                        format!(
                            "#{} [{}] {}",
                            iss["number"].as_u64().unwrap_or(0),
                            iss["state"].as_str().unwrap_or_default(),
                            iss["title"].as_str().unwrap_or_default()
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        "checks_for_ref" => {
            let runs = body_json["check_runs"]
                .as_array()
                .map(Vec::as_slice)
                .unwrap_or_default();
            if runs.is_empty() {
                "No check runs.".into()
            } else {
                runs.iter()
                    .map(|r| {
                        let name = r["name"].as_str().unwrap_or_default();
                        let status = r["status"].as_str().unwrap_or_default();
                        let concl = r["conclusion"].as_str().unwrap_or("-");
                        format!("{name}: {status} ({concl})")
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        }
        "create_issue_comment" => {
            let id = body_json["id"].as_u64().unwrap_or(0);
            let url = body_json["html_url"].as_str().unwrap_or_default();
            format!("Created comment {id}\n{url}")
        }
        _ => body_json.to_string(),
    };
    let structured = if body_json.is_null() {
        None
    } else {
        Some(body_json)
    };
    Ok((summary, structured))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn builds_requests_and_rejects_path_traversal() {
        let r = build_request(
            "create_pull_request",
            &json!({
                "owner": "acme",
                "repo": "demo",
                "title": "Add feature",
                "head": "feat",
                "base": "main",
                "draft": true
            }),
            Some("tok"),
        )
        .unwrap();
        assert_eq!(r.method, "POST");
        assert_eq!(r.url, "https://api.github.com/repos/acme/demo/pulls");
        assert!(
            r.headers
                .iter()
                .any(|(k, v)| k == "authorization" && v == "Bearer tok")
        );
        let body: Value = serde_json::from_slice(&r.body).unwrap();
        assert_eq!(body["draft"], true);

        assert!(
            build_request(
                "pull_requests_list",
                &json!({ "owner": "../evil", "repo": "demo" }),
                None
            )
            .is_err()
        );
        assert!(
            build_request(
                "checks_for_ref",
                &json!({ "owner": "acme", "repo": "demo", "ref": "main?x=1" }),
                None
            )
            .is_err()
        );
    }

    #[test]
    fn reports_rate_limits_without_retrying() {
        let resp = Response {
            status: 429,
            headers: vec![("retry-after".into(), "42".into())],
            body: br#"{"message":"secondary rate limit"}"#.to_vec(),
        };
        let err = parse_response("issues_list", &resp).unwrap_err();
        assert!(err.contains("42"), "{err}");

        let resp2 = Response {
            status: 403,
            headers: vec![
                ("x-ratelimit-remaining".into(), "0".into()),
                ("x-ratelimit-reset".into(), "1795000000".into()),
            ],
            body: br#"{"message":"API rate limit exceeded"}"#.to_vec(),
        };
        let err2 = parse_response("issues_list", &resp2).unwrap_err();
        assert!(err2.contains("1795000000"), "{err2}");
    }
}
