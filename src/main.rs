use agnt::agent::Agent;
use agnt::backend::Backend;
use agnt::{builtins, store, tool};
use std::io::{self, BufRead, Write};

const DEFAULT_SYSTEM: &str = "You are a helpful, concise assistant. When you need to act on files, directories, URLs, or search for text, PREFER the specialized tools (read_file, write_file, edit_file, list_dir, glob, grep, fetch) over the shell tool — they are faster, deterministic, and sub-millisecond. Only reach for shell when no specialized tool fits (e.g. git, cargo, systemctl, kubectl).";

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut session = "default".to_string();
    let mut system = DEFAULT_SYSTEM.to_string();
    let mut db_path = default_db();
    let mut no_stream = false;
    let mut no_db = false;
    let mut tool_allowlist: Option<Vec<String>> = std::env::var("AGNT_TOOLS")
        .ok()
        .map(|s| s.split(',').map(|t| t.trim().to_string()).collect());

    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--session" => {
                i += 1;
                session = args.get(i).cloned().unwrap_or_default();
            }
            "--system" => {
                i += 1;
                system = args.get(i).cloned().unwrap_or_default();
            }
            "--db" => {
                i += 1;
                db_path = args.get(i).cloned().unwrap_or_default();
            }
            "--tools" => {
                i += 1;
                let csv = args.get(i).cloned().unwrap_or_default();
                tool_allowlist = Some(csv.split(',').map(|t| t.trim().to_string()).collect());
            }
            "--no-stream" => no_stream = true,
            "--no-db" => no_db = true,
            "-h" | "--help" => {
                print_help();
                return;
            }
            a => {
                eprintln!("unknown arg: {}", a);
                return;
            }
        }
        i += 1;
    }

    let model = std::env::var("AGNT_MODEL").unwrap_or_else(|_| "gemma4:e4b".into());
    let backend_kind = std::env::var("AGNT_BACKEND").unwrap_or_else(|_| "ollama".into());
    let backend = match backend_kind.as_str() {
        "openai" => Backend::openai(&model, &env_required("OPENAI_API_KEY")),
        "anthropic" => Backend::anthropic(&model, &env_required("ANTHROPIC_API_KEY")),
        _ => Backend::ollama(&model),
    };

    let mut agent = Agent::new(backend, &system);
    agent.stream = !no_stream;

    if !no_db {
        match store::Store::open(&db_path) {
            Ok(s) => {
                if let Err(e) = agent.attach_store(s, &session) {
                    eprintln!("store attach: {}", e);
                }
            }
            Err(e) => eprintln!("store open: {}", e),
        }
    }

    let unsafe_shell = std::env::var("AGNT_UNSAFE_SHELL").is_ok();
    let all_tools: Vec<Box<dyn tool::Tool>> = vec![
        Box::new(builtins::ReadFile),
        Box::new(builtins::WriteFile),
        Box::new(builtins::EditFile),
        Box::new(builtins::ListDir),
        Box::new(builtins::Glob),
        Box::new(builtins::Grep),
        Box::new(builtins::Fetch),
        Box::new(builtins::Shell { unsafe_mode: unsafe_shell }),
    ];
    for t in all_tools {
        if let Some(list) = &tool_allowlist {
            if !list.iter().any(|n| n == t.name()) {
                continue;
            }
        }
        agent.tools.register(t);
    }

    println!(
        "agnt-rs — backend: {}  model: {}  session: {}",
        backend_kind, model, session
    );
    println!("tools: {}", agent.tools.names().join(", "));
    println!("(empty line to quit, /clear to reset, /stats for tool latency)");

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    loop {
        print!("\n> ");
        stdout.flush().ok();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).is_err() {
            break;
        }
        let line = line.trim().to_string();
        if line.is_empty() {
            break;
        }
        if line == "/clear" {
            agent.messages.truncate(1);
            if let Some(s) = &agent.store {
                let _ = s.clear(&agent.session);
                for m in &agent.messages {
                    let _ = s.append(&agent.session, m);
                }
            }
            println!("(cleared)");
            continue;
        }
        if line == "/stats" {
            match &agent.store {
                Some(s) => match s.stats(&agent.session) {
                    Ok(rows) if rows.is_empty() => println!("(no tool calls in this session yet)"),
                    Ok(rows) => {
                        println!("{:<12} {:>6} {:>12} {:>12}", "tool", "count", "avg_us", "max_us");
                        for (name, count, avg, max) in rows {
                            println!("{:<12} {:>6} {:>12} {:>12}", name, count, avg, max);
                        }
                    }
                    Err(e) => eprintln!("stats: {}", e),
                },
                None => println!("(persistence disabled — no stats available)"),
            }
            continue;
        }
        match agent.step(&line) {
            Ok(out) => {
                if !agent.stream && !out.is_empty() {
                    println!("{}", out);
                }
            }
            Err(e) => eprintln!("\nerror: {}", e),
        }
    }
}

fn env_required(k: &str) -> String {
    std::env::var(k).unwrap_or_else(|_| {
        eprintln!("missing env: {}", k);
        std::process::exit(1);
    })
}

fn default_db() -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    format!("{}/.agnt-rs.db", home)
}

fn print_help() {
    println!("agnt-rs — dense rust agent");
    println!("  --session <id>    session id (default: default)");
    println!("  --system <text>   system prompt (ignored if session has history)");
    println!("  --db <path>       sqlite path (default: ~/.agnt-rs.db)");
    println!("  --tools <csv>     comma-separated allowlist (e.g. read_file,grep,glob)");
    println!("  --no-stream       disable streaming output");
    println!("  --no-db           disable persistence");
    println!();
    println!("REPL commands: /clear, /stats, (empty line to quit)");
    println!();
    println!("env: AGNT_MODEL, AGNT_BACKEND=ollama|openai|anthropic,");
    println!("     AGNT_TOOLS (csv allowlist), AGNT_UNSAFE_SHELL (disable denylist),");
    println!("     OPENAI_API_KEY, ANTHROPIC_API_KEY");
}
