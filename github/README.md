# GitHub (`kumikit.github`)

MCP tools for GitHub pull requests, issues and commit checks over the GitHub REST API
(`https://api.github.com`), using only outgoing HTTPS and a fine-grained personal access
token (`github-token`).

## Setup

Set a fine-grained GitHub personal access token on the plugin's Settings page in Kumikit
(or via `secret.set { name: "github-token", value: "..." }`). Write tools
(`create_pull_request`, `create_issue_comment`) always require approval in the running turn.
