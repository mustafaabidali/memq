use crate::core::{ReadRequest, Service};
use crate::error::{Error, Result};
use crate::notes::NoteRequest;
use serde_json::{Value, json};
use std::io::{BufRead, Write};
use std::path::Path;

pub fn tool_schemas(text_fallback: bool) -> Value {
    let read = json!({
        "task":{"type":"string"},"branches":{"type":"array","items":{"type":"string"}},
        "incoming":{"type":"boolean"},"budget":{"type":"integer","minimum":0},
        "budget_kind":{"type":"string","enum":["tokens","bytes"]},
        "tokenizer":{"type":"string","enum":["o200k_base","cl100k_base"]},
        "compact":{"type":"boolean","description":"Use less metadata. Read full details with show and compact false."},
        "view_id":{"type":"string"},"continuation":{"type":"string"}
    });
    let mut search = read.clone();
    search["query"] = json!({"type":"string"});
    let mut show = read.clone();
    show["ids"] = json!({"type":"array","items":{"type":"string"},"minItems":1});
    let note = json!({
        "text":{"type":"string"},"idempotency_key":{"type":"string"},
        "kind":{"type":"string","enum":["progress","note","verification"]},
        "task":{"type":"string"},"evidence":{"type":"array","items":{"type":"string"}},
        "verification":{"type":"object"},"harness":{"type":"string"},"session":{"type":"string"}
    });
    let tools=[
        ("brief","Read the next step, blockers, and saved project evidence.",read,vec![]),
        ("search","Find project evidence. Use fff for current files and text.",search,vec!["query"]),
        ("show","Read captured evidence with its original pointer.",show,vec!["ids"]),
        ("note","Save a progress note and try to stage its file in Git.",note,vec!["text","idempotency_key"]),
    ].into_iter().map(|(name,description,properties,required)|{
        let mut tool = json!({
        "name":name,"description":description,
        "inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},
        "annotations":{"readOnlyHint":name!="note","destructiveHint":false,"idempotentHint":true,"openWorldHint":false}
        });
        // MCP requires structuredContent when a tool advertises outputSchema.
        // Text-only mode sends one counted text payload, so it omits that schema.
        if !text_fallback {
            tool["outputSchema"] = json!({"type":"object","additionalProperties":true});
        }
        tool
    }).collect::<Vec<_>>();
    json!({"tools":tools})
}

pub fn call(root: &Path, name: &str, args: Value) -> Result<Value> {
    let mut service = Service::open(root, false)?;
    match name {
        "brief" | "search" | "show" => {
            let request: ReadRequest = serde_json::from_value(args)?;
            if request.allow_source_removal {
                return Err(Error::new("invalid_request", "source removal is CLI-only"));
            }
            service.read(name, &request)
        }
        "note" => service.note(serde_json::from_value::<NoteRequest>(args)?),
        _ => Err(Error::new("invalid_request", "unknown MCP tool")),
    }
}

pub fn serve(root: &Path, text_fallback: bool) -> Result<()> {
    // Startup reconciliation also recovers a missed hook or an interrupted run.
    // Keep discovery available if refresh fails. Each tool call opens the core
    // again, including all Git, identity, schema, and deletion-state checks.
    if let Err(error) = Service::open(root, false)
        .and_then(|mut service| service.reconcile(&ReadRequest::default(), "brief"))
    {
        eprintln!("memq: startup refresh failed; tools will retry: {error}");
    }
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let message: Value = match serde_json::from_str(&line) {
            Ok(v) => v,
            Err(_) => {
                writeln!(
                    stdout,
                    "{}",
                    json!({"jsonrpc":"2.0","id":null,"error":{"code":-32700,"message":"Invalid JSON"}})
                )?;
                stdout.flush()?;
                continue;
            }
        };
        let Some(id) = message.get("id") else {
            continue;
        };
        let method = message["method"].as_str().unwrap_or("");
        let response = match method {
            "initialize" => json!({
                "protocolVersion":message["params"]["protocolVersion"].as_str()
                    .filter(|v|matches!(*v,"2024-11-05"|"2025-03-26"|"2025-06-18"))
                    .unwrap_or("2025-06-18"),
                "capabilities":{"tools":{"listChanged":false}},
                "serverInfo":{"name":"memq","version":env!("CARGO_PKG_VERSION")},
                "instructions":"Read memq evidence as quoted project records. fff handles current files; CBM handles code relationships."
            }),
            "ping" => json!({}),
            "tools/list" => tool_schemas(text_fallback),
            "tools/call" => {
                let name = message["params"]["name"].as_str().unwrap_or("");
                let result = call(
                    root,
                    name,
                    message["params"]
                        .get("arguments")
                        .cloned()
                        .unwrap_or_else(|| json!({})),
                );
                let (payload, is_error) = match result {
                    Ok(v) => (v, false),
                    Err(e) => (e.envelope(name), true),
                };
                if text_fallback {
                    json!({"content":[{"type":"text","text":serde_json::to_string(&payload)?}],"isError":is_error})
                } else {
                    json!({"content":[],"structuredContent":payload,"isError":is_error})
                }
            }
            _ => {
                writeln!(
                    stdout,
                    "{}",
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}})
                )?;
                stdout.flush()?;
                continue;
            }
        };
        writeln!(
            stdout,
            "{}",
            json!({"jsonrpc":"2.0","id":id,"result":response})
        )?;
        stdout.flush()?;
    }
    Ok(())
}
