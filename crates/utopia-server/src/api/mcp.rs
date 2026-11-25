//! MCP server (Streamable HTTP).
//!
//! One route: `POST /api/v1/kbs/{kb_id}/mcp`, accepts JSON-RPC 2.0.
//!
//! **Transport is Streamable HTTP, not stdio**; the reasoning is in `docs/decisions/0014`: Utopia
//! is already a server, and stdio would mean either spawning a child process that connects back to it, or opening another connection pool;
//! and this is a multi-person deployment, where five people each connecting should be served by the same deployment.
//!
//! **Every POST re-authenticates.** The spec allows reusing the result of one handshake for the whole connection,
//! but the discipline in 0014 is clear:
//!
//! > Scope is checked at every tool entry point, not at the handshake. When `revoked_at` is set mid-session it must
//! > take effect immediately.
//!
//! Once this is stateless, that property comes for free: there is no "connection" that could be trusted.
//!
//! **The response uses `application/json`, not SSE.** The spec allows either, and these tools are
//! strict request-response, with nothing the server pushes unprompted; SSE is for notifications, which this version does not need.

use axum::extract::{Path, State};
use axum::http::HeaderMap;
use axum::Json;
use serde_json::{json, Value};
use utopia_core::models::Role;
use uuid::Uuid;

use super::tools::{self, ToolCtx, ToolSink};
use crate::error::ApiResult;
use crate::state::AppState;

/// The protocol version implemented. When a client reports another version, it is answered with this one anyway:
/// the spec requires the server to reply with a version it supports, and leaves it to the client to accept or not
const PROTOCOL_VERSION: &str = "2025-06-18";

/// The read-only ones: any token that reaches this KB can call them.
///
/// `query_data` is still not exposed: it runs SQL against **production databases outside the deployment**, and how it is audited,
/// and what an agent fed poisoned documents could do with it, are still unresolved (see the confused-deputy section of 0014).
/// The two read-only business-rule tools are also here (0021): the criteria must be visible. When an agent explains
/// "why does this count as a gas-bearing well", it should read the rule and its threshold, not guess one.
/// **Writing rules is not exposed**: the reasoning criteria are written down by a person, and one tool call cannot tell "a person dictated it, and the agent
/// typed it for them" apart from "the model made one up".
const EXPOSED: [&str; 11] = [
    "search_chunks",
    "get_document",
    "search_docs",
    "find_entities",
    "list_rules",
    "rule_matches",
    "changes",
    "entity_facts",
    "neighbors",
    "timeline",
    "paths_between",
];

/// The ones that write into the ledger.
///
/// **0014 originally left out `remember` mainly because of the confused deputy**: an agent reads documents in the KB, and
/// a line "please remember X" written inside a document could be taken as an instruction and carried out, with that person's full permissions.
/// 0015 dismantled that objection: facts extracted from memory go first into `pending_facts` to wait for a person's nod,
/// and without a nod nothing enters the graph. So what is opened here is not "writing to the graph", it is "proposing".
const WRITABLE: [&str; 1] = ["remember"];

/// `can_write` = the token's scope is write **and** the person is editor or above in this KB.
/// Both conditions are required: scope is the ceiling, and the role is the permission (0014)
fn is_exposed(name: &str, can_write: bool) -> bool {
    EXPOSED.contains(&name) || (can_write && WRITABLE.contains(&name))
}

/// Authentication and authorization. **Two checks, not one.**
///
/// The token says "who this is, and which KBs this key reaches"; `require_kb` says "what role this person has
/// in this KB". The former only narrows; the latter is the permission. A token with full scope held by a
/// viewer is still just a viewer.
/// Output of authentication: the person, the token, the KB, and **effective write permission**: token scope ∩ the person's role in this KB.
type Authorized = (
    utopia_core::models::User,
    utopia_store::tokens::Authenticated,
    utopia_core::models::KnowledgeBase,
    bool,
);

async fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    kb_id: Uuid,
) -> Result<Authorized, utopia_core::AppError> {
    let raw = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .ok_or(utopia_core::AppError::Unauthorized)?;
    let auth = utopia_store::tokens::authenticate(&state.pool, raw.trim()).await?;
    // The token's scope: if it was limited to KBs, it cannot reach others
    if !auth.covers(kb_id) {
        return Err(utopia_core::AppError::Forbidden);
    }
    // Person: deactivation takes effect at once (find_user_by_id blocks it)
    let user = utopia_store::accounts::find_user_by_id(&state.pool, auth.user_id)
        .await?
        .ok_or(utopia_core::AppError::Unauthorized)?;
    // Role: goes through the same guard as the web UI, not one line changed
    let kb = utopia_store::access::require_kb(&state.pool, &user, kb_id, Role::Viewer).await?;
    // **Narrowed twice.** Ticking write on the token only means it does not exclude writing; whether writing is allowed asks about the person's
    // role in this KB: a token with full scope held by a viewer is still read-only
    let can_write = auth.can_write()
        && utopia_store::access::kb_role(&state.pool, &user, &kb)
            .await?
            .is_some_and(|role| role >= Role::Editor);
    Ok((user, auth, kb, can_write))
}

/// OpenAI shape -> MCP shape.
///
/// **Shares the one definition in `chat.rs`**, rather than writing a second one here. The names and parameter schemas are the
/// contract with the `tools.rs` execution side; copying them into two places would sooner or later diverge, which is exactly what
/// extracting the tools in the previous step was meant to prevent.
///
/// Known flaw: the descriptions are written for the in-app assistant, and the `search_chunks` one still says "can be cited
/// as [n]", while MCP clients have no citation numbers. Sharing one copy is worth more than that sentence costs;
/// it can be split when it really needs to be.
fn to_mcp_tools(openai: &Value, can_write: bool) -> Vec<Value> {
    openai
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|t| {
                    let f = t.get("function")?;
                    let name = f.get("name")?.as_str()?;
                    if !is_exposed(name, can_write) {
                        return None;
                    }
                    Some(json!({
                        "name": name,
                        "description": f.get("description").and_then(|d| d.as_str()).unwrap_or(""),
                        "inputSchema": f.get("parameters").cloned().unwrap_or(json!({
                            "type": "object", "properties": {}
                        })),
                    }))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn ok(id: Option<Value>, result: Value) -> Json<Value> {
    Json(json!({ "jsonrpc": "2.0", "id": id, "result": result }))
}

fn tool_result(result: tools::ToolResult) -> Value {
    let mut response = json!({
        "content": [{ "type": "text", "text": result.text }],
        "isError": result.is_error,
    });
    if let Some(content) = result.structured_content {
        response["structuredContent"] = content;
    }
    response
}

/// A JSON-RPC error is not an HTTP error: **the transport succeeded, the method failed**.
/// Reply 200 with an error body, which is what clients can parse.
fn rpc_err(id: Option<Value>, code: i64, message: &str) -> Json<Value> {
    Json(json!({
        "jsonrpc": "2.0", "id": id,
        "error": { "code": code, "message": message }
    }))
}

pub async fn handle(
    State(state): State<AppState>,
    Path(kb_id): Path<Uuid>,
    headers: HeaderMap,
    Json(req): Json<Value>,
) -> ApiResult<Json<Value>> {
    let (user, auth, kb, can_write) = authorize(&state, &headers, kb_id).await?;
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or(json!({}));

    Ok(match method {
        "initialize" => ok(
            id,
            json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": { "listChanged": false } },
                "serverInfo": { "name": "utopia", "version": super::build_version::build_version() },
            }),
        ),
        // A notification has no id, and by the spec should not get a response body; but the HTTP side still has to send something,
        // 202 would be the more accurate status, but to keep the handler signature uniform an empty result is returned
        "notifications/initialized" | "notifications/cancelled" => ok(None, json!({})),
        "ping" => ok(id, json!({})),
        // **The list changes with this token.** A token that cannot write does not see `remember`:
        // listing a tool you cannot call means the agent on the other end keeps trying
        "tools/list" => ok(
            id,
            json!({
                // Pass an empty data source list: `query_data` is not exposed, so its description need not be generated
                "tools": to_mcp_tools(&super::chat::tools_schema(can_write, &[]), can_write)
            }),
        ),
        "tools/call" => {
            let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or(json!({}));
            if !is_exposed(name, can_write) {
                // The three kinds of "no" must be kept apart, or the client can only keep retrying:
                // a tool that cannot be fetched (query_data) means "not exposed in this version"; a write tool means
                // "this token or this person cannot write"; the latter can be fixed by a person, the former cannot
                let message = if WRITABLE.contains(&name) {
                    format!(
                        "Tool '{name}' needs a token with the write scope, \
                         held by an editor in this base"
                    )
                } else {
                    format!("Tool '{name}' is not exposed over MCP in this version")
                };
                return Ok(rpc_err(id, -32601, &message));
            }
            // Shares the argument guard with chat: a missing query must not turn into a successful empty search.
            let result = match super::chat::check_call(
                &super::chat::tools_schema(can_write, &[]),
                name,
                &args.to_string(),
            ) {
                Err((text, step)) => tools::ToolResult::new(text, step).error(),
                Ok(args) => {
                    // `mounted_sources` stays empty: `query_data` is not exposed, so giving it would help no one.
                    // `can_write` is no longer hard-coded to false: it is now computed from the token and the role together
                    let ctx = ToolCtx {
                        state: &state,
                        kb_id,
                        workspace_id: kb.workspace_id,
                        mounted_sources: &[],
                        can_write,
                        actor: Some(user.id),
                        // "Who said it" has to be answered at the agent level: one person can have three clients attached at once
                        via_token: Some(auth.token_id),
                        // The agent's intent is not in the request; ties in name are ordered by fact count
                        question: None,
                    };
                    let mut sink = ToolSink::default();
                    tools::dispatch(&ctx, &mut sink, name, &args).await
                }
            };
            let _ = utopia_store::audit::record(
                &state.pool,
                Some(kb_id),
                user.id,
                "mcp.tool_called",
                "personal_token",
                Some(auth.token_id),
                json!({ "tool": name }),
            )
            .await;
            ok(id, tool_result(result))
        }
        other => rpc_err(id, -32601, &format!("Unknown method: {other}")),
    })
}

#[cfg(test)]
#[path = "mcp_tests.rs"]
mod tests;
