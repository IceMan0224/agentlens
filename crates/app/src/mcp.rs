//! `agentlens mcp`: the MCP server agents start once it is registered in Settings. It speaks
//! JSON-RPC over stdio and answers by asking the running dashboard, which keeps the lineage and
//! decides what is safe to end. It exits when the agent closes its input.

use std::io::{BufRead, Write};
use std::time::Duration;

use agentlens_core::mcp::inbox::{self, Reply, Request};
use serde_json::{Value, json};

/// Used when the agent names no version of its own.
const PROTOCOL: &str = "2025-06-18";
/// The dashboard answers within a refresh; longer means it is not running.
const TIMEOUT: Duration = Duration::from_secs(5);

const INSTRUCTIONS: &str = "agentlens watches the processes coding agents start. Use it to find \
processes that ended agent sessions left behind (MCP servers, dev servers, shells) and to end them \
safely. It needs the agentlens dashboard running on this machine.";

pub fn serve() -> anyhow::Result<()> {
    let dir = inbox::dir();
    let ask = |request: Request| match &dir {
        Some(dir) => inbox::ask(dir, &request, TIMEOUT),
        None => Err("no state directory".into()),
    };
    let mut out = std::io::stdout().lock();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(reply) = handle(&line, &ask) {
            writeln!(out, "{reply}")?;
            out.flush()?;
        }
    }
    Ok(())
}

/// The reply to one message; notifications and responses get none.
fn handle(line: &str, ask: &impl Fn(Request) -> Result<Reply, String>) -> Option<Value> {
    let Ok(msg) = serde_json::from_str::<Value>(line) else {
        return Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}}));
    };
    let id = msg.get("id")?.clone();
    let method = msg.get("method")?.as_str().unwrap_or_default();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    let result = match method {
        "initialize" => Ok(json!({
            "protocolVersion": params["protocolVersion"].as_str().unwrap_or(PROTOCOL),
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "agentlens", "version": env!("CARGO_PKG_VERSION")},
            "instructions": INSTRUCTIONS,
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({"tools": tools()})),
        "tools/call" => call(&params, ask),
        _ => Err((-32601, format!("unknown method {method}"))),
    };
    Some(match result {
        Ok(result) => json!({"jsonrpc": "2.0", "id": id, "result": result}),
        Err((code, message)) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message}}),
    })
}

fn tools() -> Value {
    let none = json!({"type": "object", "properties": {}});
    json!([
        {
            "name": "list_leftovers",
            "description": "List processes that ended agent sessions left behind, and other cleanup candidates, with the evidence for each and whether ending it is proposed.",
            "inputSchema": none,
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "list_session_processes",
            "description": "List the processes this agent session is running (its process tree), with memory, command and listening ports.",
            "inputSchema": none,
            "annotations": {"readOnlyHint": true},
        },
        {
            "name": "end_leftovers",
            "description": "End cleanup candidates from list_leftovers, each with its child processes. Without pids, ends the proposed ones. Only current candidates can be ended, never this session.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "pids": {"type": "array", "items": {"type": "integer"}, "description": "Root pids from list_leftovers"}
                },
            },
            "annotations": {"destructiveHint": true},
        },
    ])
}

fn call(params: &Value, ask: &impl Fn(Request) -> Result<Reply, String>) -> Result<Value, (i64, String)> {
    let name = params["name"].as_str().unwrap_or_default();
    if !tools().as_array().is_some_and(|t| t.iter().any(|t| t["name"] == name)) {
        return Err((-32602, format!("unknown tool {name}")));
    }
    let request = Request {
        tool: name.to_string(),
        args: params.get("arguments").cloned().unwrap_or(json!({})),
        caller: std::process::id(),
    };
    let reply = ask(request).unwrap_or_else(|_| Reply {
        text: "The agentlens dashboard is not running, so this cannot be checked. Ask the user to start \
               `agentlens` in a terminal and try again."
            .into(),
        error: true,
    });
    Ok(json!({"content": [{"type": "text", "text": reply.text}], "isError": reply.error}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn echo(request: Request) -> Result<Reply, String> {
        Ok(Reply {
            text: format!("{} {}", request.tool, request.args),
            error: false,
        })
    }

    #[test]
    fn speaks_mcp() {
        let init = handle(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{}}}"#,
            &echo,
        )
        .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["id"], 1);
        assert!(handle(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#, &echo).is_none());
        let list = handle(r#"{"jsonrpc":"2.0","id":"a","method":"tools/list"}"#, &echo).unwrap();
        assert_eq!(list["result"]["tools"].as_array().unwrap().len(), 3);
        let call = handle(
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"end_leftovers","arguments":{"pids":[5]}}}"#,
            &echo,
        )
        .unwrap();
        assert_eq!(call["result"]["content"][0]["text"], r#"end_leftovers {"pids":[5]}"#);
        assert_eq!(call["result"]["isError"], false);
        let unknown = handle(
            r#"{"jsonrpc":"2.0","id":3,"method":"tools/call","params":{"name":"rm"}}"#,
            &echo,
        )
        .unwrap();
        assert_eq!(unknown["error"]["code"], -32602);
        assert_eq!(handle("not json", &echo).unwrap()["error"]["code"], -32700);
    }

    #[test]
    fn says_so_when_no_dashboard_answers() {
        let call = handle(
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/call","params":{"name":"list_leftovers"}}"#,
            &|_| Err("no dashboard answered".into()),
        )
        .unwrap();
        assert_eq!(call["result"]["isError"], true);
        assert!(
            call["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("not running")
        );
    }
}
