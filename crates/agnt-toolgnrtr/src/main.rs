//! `toolgnrtr` CLI — drives generation, listing, testing, and stats from the
//! command line. Backend selection mirrors `agnt-net::Backend`: pick one of
//! ollama/openai/anthropic and supply a model.

use agnt_net::Backend;
use agnt_toolgnrtr::{SandboxConfig, ToolGenerator};
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::Value;

#[derive(Parser, Debug)]
#[command(name = "toolgnrtr")]
#[command(about = "Agent tool factory — generate, sandbox, test, and version tools at runtime")]
struct Cli {
    /// Path to the SQLite database used as the tool store.
    #[arg(long, default_value = "toolgnrtr.db", env = "TOOLGNRTR_DB")]
    db: String,

    /// Backend provider for the generator LLM.
    #[arg(long, value_enum, default_value_t = BackendKind::Ollama, env = "TOOLGNRTR_BACKEND")]
    backend: BackendKind,

    /// Model name passed to the backend.
    #[arg(long, default_value = "gemma4:e4b", env = "TOOLGNRTR_MODEL")]
    model: String,

    /// API key for openai/anthropic. Read from environment if not supplied.
    #[arg(long, env = "TOOLGNRTR_API_KEY")]
    api_key: Option<String>,

    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum BackendKind {
    Ollama,
    Openai,
    Anthropic,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Generate a new tool from a natural-language description.
    Generate {
        /// Description of what the tool should do.
        description: String,
        #[arg(long)]
        allow_network: bool,
        #[arg(long, default_value_t = 30_000)]
        max_runtime_ms: u64,
        #[arg(long, default_value_t = 65_536)]
        max_output_bytes: usize,
        #[arg(long, default_value_t = 64 * 1024 * 1024)]
        max_memory_bytes: usize,
    },
    /// Substring search over name + description.
    Search {
        query: String,
    },
    /// List the latest version of every stored tool.
    List,
    /// Print the source + schema of a stored tool.
    Show {
        name: String,
    },
    /// Run a stored tool against a JSON input.
    Test {
        name: String,
        /// JSON object. Defaults to `{}`.
        #[arg(long, default_value = "{}")]
        input: String,
    },
    /// Generate a revised version of a tool from feedback.
    Evolve {
        name: String,
        feedback: String,
    },
    /// Print aggregate call stats for one tool or for everything.
    Stats {
        #[arg(long)]
        name: Option<String>,
    },
}

fn build_backend(kind: BackendKind, model: &str, api_key: Option<String>) -> Result<Backend, String> {
    match kind {
        BackendKind::Ollama => Ok(Backend::ollama(model)),
        BackendKind::Openai => {
            let key = api_key
                .or_else(|| std::env::var("OPENAI_API_KEY").ok())
                .ok_or_else(|| "missing API key for openai".to_string())?;
            let mut b = Backend::openai(model, &key);
            if let Ok(base) = std::env::var("OPENAI_API_BASE") {
                b = b.with_base_url(&base);
            }
            Ok(b)
        }
        BackendKind::Anthropic => {
            let key = api_key
                .or_else(|| std::env::var("ANTHROPIC_API_KEY").ok())
                .ok_or_else(|| "missing API key for anthropic".to_string())?;
            Ok(Backend::anthropic(model, &key))
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), String> {
    let cli = Cli::parse();
    let backend = build_backend(cli.backend, &cli.model, cli.api_key)?;
    let gen = ToolGenerator::new(backend, &cli.db)?;

    match cli.cmd {
        Cmd::Generate {
            description,
            allow_network,
            max_runtime_ms,
            max_output_bytes,
            max_memory_bytes,
        } => {
            let sandbox = SandboxConfig {
                allow_network,
                max_runtime_ms,
                max_output_bytes,
                max_memory_bytes,
            };
            let tool = gen.generate(&description, sandbox)?;
            println!("generated: {}", tool.name);
            println!("description: {}", tool.description);
            println!("source:\n{}", tool.source);
        }
        Cmd::Search { query } => {
            for s in gen.search(&query)? {
                println!("{} v{} — {}", s.name, s.version, s.description);
            }
        }
        Cmd::List => {
            for s in gen.list()? {
                println!("{} v{} — {}", s.name, s.version, s.description);
            }
        }
        Cmd::Show { name } => {
            let rec = gen
                .store()
                .load_tool(&name)?
                .ok_or_else(|| format!("no such tool: {name}"))?;
            println!("name: {}", rec.name);
            println!("version: {}", rec.version);
            println!("description: {}", rec.description);
            println!(
                "schema: {}",
                serde_json::to_string_pretty(&rec.schema).unwrap_or_default()
            );
            println!("source:\n{}", rec.source);
        }
        Cmd::Test { name, input } => {
            let parsed: Value =
                serde_json::from_str(&input).map_err(|e| format!("parse input json: {e}"))?;
            let out = gen.test(&name, parsed)?;
            println!("{out}");
        }
        Cmd::Evolve { name, feedback } => {
            let tool = gen.evolve(&name, &feedback)?;
            println!("evolved: {}", tool.name);
            println!("source:\n{}", tool.source);
        }
        Cmd::Stats { name } => {
            let stats = gen.stats(name.as_deref())?;
            println!("calls: {}", stats.calls);
            println!("failures: {}", stats.failures);
            println!("avg_duration_us: {:.2}", stats.avg_duration_us);
            if let Some(ts) = stats.last_called_at {
                println!("last_called_at: {ts}");
            }
        }
    }
    Ok(())
}
