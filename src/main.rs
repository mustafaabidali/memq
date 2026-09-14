use clap::{Args, Parser, Subcommand};
use memq::core::{ReadRequest, Service};
use memq::error::{Error, Result};
use memq::notes::NoteRequest;
use memq::tombstone::Predicate;
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "memq",
    version,
    about = "Pick up work with saved project context and source evidence"
)]
struct Cli {
    /// Git project directory. Defaults to the current directory.
    #[arg(long, global = true, default_value = ".")]
    repo: PathBuf,
    /// Kept for scripts. All commands already return JSON.
    #[arg(long, global = true)]
    json: bool,
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Set up memq in this Git project.
    Init,
    /// Refresh saved records from their sources.
    Reconcile {
        #[command(flatten)]
        read: ReadArgs,
        #[arg(long)]
        allow_source_removal: bool,
    },
    /// Read the next step, blockers, decisions, and recent progress.
    Brief(ReadArgs),
    /// Find saved project records.
    Search {
        query: String,
        #[command(flatten)]
        read: ReadArgs,
    },
    /// Read full records and their source evidence.
    Show {
        #[arg(required = true)]
        ids: Vec<String>,
        #[command(flatten)]
        read: ReadArgs,
    },
    /// Save a progress note and try to stage its file in Git.
    Note {
        #[arg(long)]
        text: String,
        #[arg(long)]
        idempotency_key: String,
        #[arg(long, default_value = "progress")]
        kind: String,
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        evidence: Vec<String>,
        #[arg(long)]
        verification: Option<PathBuf>,
        #[arg(long)]
        harness: Option<String>,
        #[arg(long)]
        session: Option<String>,
    },
    /// Serve brief, search, show, and note over MCP on stdin and stdout.
    Mcp {
        #[arg(long)]
        text_fallback: bool,
    },
    /// Check the local store and optional harness setup.
    Doctor {
        #[arg(long)]
        gc: bool,
        #[arg(long)]
        probe_harnesses: bool,
    },
    /// Read new records from configured local harness stores.
    Capture,
    /// Prepare semantic search with the configured embedding command.
    Embed,
    /// Measure storage and token costs with made-up records.
    Measure {
        #[arg(long, default_value_t = 2000)]
        records: usize,
        /// Count bytes and tokens in an existing UTF-8 text file.
        #[arg(long)]
        input: Option<PathBuf>,
        #[arg(long, default_value = "o200k_base")]
        tokenizer: String,
    },
    /// Read a harness event from stdin and return a fresh briefing.
    Hook {
        #[arg(long, default_value = "codex")]
        harness: String,
        #[arg(long, default_value_t = 2000)]
        budget: usize,
        /// Use less metadata in the briefing.
        #[arg(long)]
        compact: bool,
    },
    /// Rebuild the search store from sources that still exist.
    Rebuild,
    /// Stop retaining selected records. Source files stay in place.
    Forget {
        id: Option<String>,
        #[arg(long)]
        source: Option<String>,
        #[arg(long)]
        project: bool,
        #[arg(long)]
        after: Option<String>,
        #[arg(long)]
        before: Option<String>,
    },
}

#[derive(Args, Default)]
struct ReadArgs {
    /// Focus on this task or topic.
    #[arg(long)]
    task: Option<String>,
    /// Read these local branches. Separate names with commas.
    #[arg(long, value_delimiter = ',')]
    branches: Vec<String>,
    /// Include records observed on the configured remote branch.
    #[arg(long,num_args=0..=1,default_missing_value="true")]
    incoming: Option<bool>,
    /// Maximum size of the returned JSON.
    #[arg(long)]
    budget: Option<usize>,
    /// Count the budget in tokens or bytes.
    #[arg(long)]
    budget_kind: Option<String>,
    /// Token counter: o200k_base or cl100k_base.
    #[arg(long)]
    tokenizer: Option<String>,
    /// Use less metadata. Read full details with show and no --compact.
    #[arg(long)]
    compact: bool,
    /// Read a saved view. A view is one consistent set of records.
    #[arg(long)]
    view_id: Option<String>,
    /// Continue a prior response with its returned continuation value.
    #[arg(long)]
    continuation: Option<String>,
}

impl From<ReadArgs> for ReadRequest {
    fn from(a: ReadArgs) -> Self {
        Self {
            task: a.task,
            branches: a.branches,
            incoming: a.incoming,
            budget: a.budget,
            budget_kind: a.budget_kind,
            tokenizer: a.tokenizer,
            compact: a.compact,
            view_id: a.view_id,
            continuation: a.continuation,
            ..Self::default()
        }
    }
}

fn run(cli: Cli) -> Result<Option<Value>> {
    let root = &cli.repo;
    let result = match cli.command {
        Commands::Init => Service::initialize(root)?,
        Commands::Mcp { text_fallback } => {
            memq::mcp::serve(root, text_fallback)?;
            return Ok(None);
        }
        Commands::Brief(read) => Service::open(root, false)?.read("brief", &read.into())?,
        Commands::Search { query, read } => {
            let mut request: ReadRequest = read.into();
            request.query = Some(query);
            Service::open(root, false)?.read("search", &request)?
        }
        Commands::Show { ids, read } => {
            let mut request: ReadRequest = read.into();
            request.ids = ids;
            Service::open(root, false)?.read("show", &request)?
        }
        Commands::Reconcile {
            read,
            allow_source_removal,
        } => {
            let mut request: ReadRequest = read.into();
            request.allow_source_removal = allow_source_removal;
            let view = Service::open(root, false)?.reconcile(&request, "reconcile")?;
            json!({"freshness":view.meta["freshness"],"coverage":view.meta["coverage"],"items":view.members.len()})
        }
        Commands::Note {
            text,
            idempotency_key,
            kind,
            task,
            evidence,
            verification,
            harness,
            session,
        } => {
            let verification = verification
                .map(|p| {
                    std::fs::read(p)
                        .map_err(Error::from)
                        .and_then(|b| serde_json::from_slice(&b).map_err(Error::from))
                })
                .transpose()?;
            Service::open(root, false)?.note(NoteRequest {
                text,
                idempotency_key,
                kind,
                task,
                evidence,
                verification,
                harness,
                session,
            })?
        }
        Commands::Doctor {
            gc,
            probe_harnesses,
        } => Service::open(root, false)?.doctor(gc, probe_harnesses)?,
        Commands::Capture => {
            let view = Service::open(root, false)?.reconcile(&ReadRequest::default(), "capture")?;
            json!({"freshness":view.meta["freshness"],"coverage":view.meta["coverage"]})
        }
        Commands::Embed => Service::open(root, false)?.embed_pending()?,
        Commands::Measure {
            records,
            input,
            tokenizer,
        } => {
            if let Some(path) = input {
                let text = std::fs::read_to_string(path)?;
                let budget = memq::budget::Budget {
                    kind: "tokens".into(),
                    encoding: tokenizer,
                    limit: usize::MAX,
                };
                budget.validate()?;
                json!({"bytes":text.len(),"tokens":budget.count(&text),"encoding":budget.encoding})
            } else {
                memq::measure::run(records)?
            }
        }
        Commands::Hook {
            harness,
            budget,
            compact,
        } => {
            if harness != "codex" {
                return Err(Error::new("invalid_request", "unsupported hook harness"));
            }
            let input: Value = serde_json::from_reader(std::io::stdin().lock())?;
            memq::hooks::codex(root, &input, budget, compact)?
        }
        Commands::Rebuild => {
            let view =
                Service::open(root, true)?.reconcile(&ReadRequest::default(), "reconcile")?;
            json!({"freshness":view.meta["freshness"],"coverage":view.meta["coverage"]})
        }
        Commands::Forget {
            id,
            source,
            project,
            after,
            before,
        } => {
            if id.is_none() && source.is_none() && !project {
                return Err(Error::new(
                    "invalid_request",
                    "forget requires an item, --source, or --project",
                ));
            }
            let mut service = Service::open(root, false)?;
            let project_id = if id.is_none() {
                Some(service.project_id().to_owned())
            } else {
                None
            };
            service.forget(Predicate {
                item_id: id,
                project_id,
                source_id: source,
                after,
                before,
            })?
        }
    };
    Ok(Some(result))
}

fn main() {
    let cli = Cli::parse();
    let operation = match &cli.command {
        Commands::Init => "init",
        Commands::Brief(_) => "brief",
        Commands::Search { .. } => "search",
        Commands::Show { .. } => "show",
        Commands::Note { .. } => "note",
        Commands::Mcp { .. } => "mcp",
        Commands::Reconcile { .. } => "reconcile",
        Commands::Doctor { .. } => "doctor",
        Commands::Capture => "capture",
        Commands::Embed => "embed",
        Commands::Measure { .. } => "measure",
        Commands::Hook { .. } => "hook",
        Commands::Rebuild => "rebuild",
        Commands::Forget { .. } => "forget",
    };
    match run(cli) {
        // CLI emits exactly the compact payload that Budget counted. A trailing
        // newline would violate an exact byte limit. MCP owns its JSON-RPC frame.
        Ok(Some(v)) => print!("{}", serde_json::to_string(&v).expect("JSON")),
        Ok(None) => (),
        Err(e) => {
            print!("{}", e.envelope(operation));
            std::process::exit(e.exit);
        }
    }
}
