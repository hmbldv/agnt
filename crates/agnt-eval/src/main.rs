/// agnt end-to-end eval harness.
///
/// Runs progressively harder scenarios against the local LLM and emits a
/// Markdown report to stdout. Each scenario is independent so they can be
/// run sequentially with results accumulated, while later levels build on
/// artefacts written in earlier levels.
///
/// Levels:
///   L1 — pure inference (no tools)
///   L2 — single tool call (WriteFile / ReadFile)
///   L3 — two-tool chained (write then read-back verify)
///   L4 — multi-step arithmetic pipeline
///   L5 — multi-phase project (3 files, cross-references)
///   L6 — context pressure (long prompt + reasoning chain)
///   L7 — loop stress (task that tempts repetition)
///   L8 — degradation probe (fill context window, check coherence)
use agnt::{Agent, Observer, ToolCall, UsageStats};
use agnt::builtins::{Glob, Grep, ListDir, ReadFile, WriteFile};
use std::sync::{Arc, Mutex};
use std::time::Instant;

// ── Metrics observer ─────────────────────────────────────────────────────────

struct Metrics {
    tool_calls: Mutex<Vec<String>>,
    usage: Mutex<Vec<UsageStats>>,
}

impl Metrics {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            tool_calls: Mutex::new(Vec::new()),
            usage: Mutex::new(Vec::new()),
        })
    }

    fn drain(&self) -> (Vec<String>, UsageStats) {
        let tools = std::mem::take(&mut *self.tool_calls.lock().unwrap());
        let usages = std::mem::take(&mut *self.usage.lock().unwrap());
        let combined = usages.iter().fold(UsageStats::default(), |mut acc, u| {
            acc.prompt_tokens = acc.prompt_tokens.saturating_add(u.prompt_tokens);
            acc.completion_tokens = acc.completion_tokens.saturating_add(u.completion_tokens);
            acc
        });
        (tools, combined)
    }
}

impl Observer for Metrics {
    fn on_tool_start(&self, call: &ToolCall) {
        self.tool_calls
            .lock()
            .unwrap()
            .push(call.function.name.clone());
    }
    fn on_step_usage(&self, usage: UsageStats) {
        self.usage.lock().unwrap().push(usage);
    }
}

// ── Result type ───────────────────────────────────────────────────────────────

#[derive(Debug)]
struct ScenarioResult {
    name: String,
    level: u8,
    passed: bool,
    elapsed_ms: u64,
    turns: usize,
    tool_calls: Vec<String>,
    usage: UsageStats,
    reply_excerpt: String,
    failure: Option<String>,
}

// ── Agent factory ─────────────────────────────────────────────────────────────

fn make_agent(metrics: Arc<Metrics>, workdir: &str) -> Agent<agnt::Backend>
where
    Metrics: Observer,
{
    let backend = agnt::Backend::openai("gemma4-26b", &std::env::var("LITELLM_API_KEY").unwrap_or_else(|_| "none".into()))
        .with_base_url("http://localhost:8001/v1");
    let sandbox = Arc::new(agnt::FilesystemRoot::new(workdir).expect("sandbox"));
    let mut agent = Agent::new(
        backend,
        "You are a precise task-execution assistant. Complete every step \
         of the user's request fully before responding. Do not ask for \
         clarification — make reasonable assumptions and proceed. When \
         using tools, prefer a minimal number of calls to achieve the goal.",
    );
    agent.observer = metrics as Arc<dyn Observer>;
    // Disable streaming so the backend returns a full response with usage stats.
    #[allow(deprecated)]
    {
        agent.stream = false;
    }
    agent
        .tools
        .register(Box::new(ReadFile::with_sandbox(Arc::clone(&sandbox))));
    agent
        .tools
        .register(Box::new(WriteFile::with_sandbox(Arc::clone(&sandbox))));
    agent
        .tools
        .register(Box::new(ListDir::with_sandbox(Arc::clone(&sandbox))));
    agent
        .tools
        .register(Box::new(Grep::with_sandbox(Arc::clone(&sandbox))));
    agent
        .tools
        .register(Box::new(Glob::with_sandbox(Arc::clone(&sandbox))));
    agent.max_tool_result_bytes = 32 * 1024;
    agent
}

// ── Scenario runner ───────────────────────────────────────────────────────────

fn run(
    level: u8,
    name: &str,
    prompt: &str,
    metrics: Arc<Metrics>,
    workdir: &str,
    check: impl Fn(&str, &str) -> Result<(), String>,
) -> ScenarioResult {
    let mut agent = make_agent(Arc::clone(&metrics), workdir);
    let t0 = Instant::now();
    let result = agent.step(prompt);
    let elapsed_ms = t0.elapsed().as_millis() as u64;
    let turns = agent.messages.len();
    let (tool_calls, usage) = metrics.drain();

    match result {
        Ok(reply) => {
            let excerpt = reply.chars().take(300).collect::<String>();
            match check(&reply, workdir) {
                Ok(()) => ScenarioResult {
                    name: name.into(),
                    level,
                    passed: true,
                    elapsed_ms,
                    turns,
                    tool_calls,
                    usage,
                    reply_excerpt: excerpt,
                    failure: None,
                },
                Err(e) => ScenarioResult {
                    name: name.into(),
                    level,
                    passed: false,
                    elapsed_ms,
                    turns,
                    tool_calls,
                    usage,
                    reply_excerpt: excerpt,
                    failure: Some(format!("check failed: {}", e)),
                },
            }
        }
        Err(e) => ScenarioResult {
            name: name.into(),
            level,
            passed: false,
            elapsed_ms,
            turns,
            tool_calls,
            usage,
            reply_excerpt: String::new(),
            failure: Some(e),
        },
    }
}

// ── Checks ───────────────────────────────────────────────────────────────────

fn file_contains(workdir: &str, filename: &str, needle: &str) -> Result<(), String> {
    let path = std::path::Path::new(workdir).join(filename);
    let content =
        std::fs::read_to_string(&path).map_err(|e| format!("{}: {}", path.display(), e))?;
    if content.contains(needle) {
        Ok(())
    } else {
        Err(format!(
            "'{}' not found in {} (content: {:?})",
            needle,
            filename,
            &content[..content.len().min(200)]
        ))
    }
}

fn file_exists(workdir: &str, filename: &str) -> Result<(), String> {
    let path = std::path::Path::new(workdir).join(filename);
    if path.exists() {
        Ok(())
    } else {
        Err(format!("{} does not exist", path.display()))
    }
}

fn reply_contains(reply: &str, needle: &str) -> Result<(), String> {
    if reply.to_lowercase().contains(&needle.to_lowercase()) {
        Ok(())
    } else {
        Err(format!("reply missing '{}'", needle))
    }
}

// ── Report printer ────────────────────────────────────────────────────────────

fn print_report(results: &[ScenarioResult]) {
    println!("# agnt End-to-End Eval Report\n");
    println!("Model: `gemma4-26b` @ `localhost:8001`  ");
    println!("Date: `{}`\n", chrono_now());

    println!("## Summary\n");
    println!("| Level | Scenario | Pass | Time (ms) | Turns | Tools | Prompt T | Completion T |");
    println!("|-------|----------|------|-----------|-------|-------|----------|--------------|");
    for r in results {
        println!(
            "| L{} | {} | {} | {} | {} | {} | {} | {} |",
            r.level,
            r.name,
            if r.passed { "✅" } else { "❌" },
            r.elapsed_ms,
            r.turns,
            r.tool_calls.len(),
            r.usage.prompt_tokens,
            r.usage.completion_tokens,
        );
    }

    let pass = results.iter().filter(|r| r.passed).count();
    let total_prompt: u32 = results.iter().map(|r| r.usage.prompt_tokens).sum();
    let total_comp: u32 = results.iter().map(|r| r.usage.completion_tokens).sum();
    println!("\n**Passed:** {}/{}", pass, results.len());
    println!("**Total tokens:** {} prompt + {} completion = {} total\n",
        total_prompt, total_comp, total_prompt + total_comp);

    println!("## Scenario Details\n");
    for r in results {
        let status = if r.passed { "PASS" } else { "FAIL" };
        println!("### L{} — {} [{}]\n", r.level, r.name, status);
        println!("- **Time:** {}ms", r.elapsed_ms);
        println!("- **Message turns:** {}", r.turns);
        println!("- **Tools called:** {:?}", r.tool_calls);
        println!(
            "- **Tokens:** {} prompt / {} completion / {} total",
            r.usage.prompt_tokens,
            r.usage.completion_tokens,
            r.usage.total()
        );
        if let Some(f) = &r.failure {
            println!("- **Failure:** `{}`", f);
        }
        if !r.reply_excerpt.is_empty() {
            println!("\n**Reply excerpt:**");
            println!("```");
            println!("{}", r.reply_excerpt);
            println!("```");
        }
        println!();
    }

    // Degradation analysis
    if results.len() >= 2 {
        println!("## Degradation Analysis\n");
        let passed_levels: Vec<u8> = results.iter().filter(|r| r.passed).map(|r| r.level).collect();
        let first_fail = results.iter().find(|r| !r.passed);
        if let Some(f) = first_fail {
            println!("**First failure:** L{} — {}", f.level, f.name);
            if let Some(fail_msg) = &f.failure {
                println!("**Reason:** {}", fail_msg);
            }
        } else {
            println!("**All scenarios passed** — no degradation detected at this depth.");
        }
        println!("\n**Passed levels:** {:?}", passed_levels);

        // Token growth per level
        println!("\n**Token growth by level:**");
        for r in results {
            let bar = "#".repeat((r.usage.total() as usize / 100).min(80));
            println!("  L{}: {:>6}t  {}", r.level, r.usage.total(), bar);
        }
    }
}

fn chrono_now() -> String {
    // Minimal ISO date without pulling in chrono dependency
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let days = secs / 86400;
    let y400 = days / 146097;
    let d400 = days % 146097;
    let y100 = (d400 / 36524).min(3);
    let d100 = d400 - y100 * 36524;
    let y4 = d100 / 1461;
    let d4 = d100 % 1461;
    let y1 = (d4 / 365).min(3);
    let doy = d4 - y1 * 365;
    let year = (y400 * 400 + y100 * 100 + y4 * 4 + y1 + 1970) as u32;
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let months: [u64; 12] = if leap {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut rem = doy;
    let mut month = 1u32;
    for &m in &months {
        if rem < m {
            break;
        }
        rem -= m;
        month += 1;
    }
    format!("{}-{:02}-{:02}", year, month, rem + 1)
}

// ── Main ──────────────────────────────────────────────────────────────────────

fn main() {
    let workdir = "/tmp/agnt-eval";
    std::fs::create_dir_all(workdir).expect("create workdir");
    // Clean slate
    for entry in std::fs::read_dir(workdir).unwrap().flatten() {
        let _ = std::fs::remove_file(entry.path());
    }

    let mut results: Vec<ScenarioResult> = Vec::new();

    eprintln!("=== agnt-eval: running 8 levels ===\n");

    // ─────────────────────────────────────────────────────────────────────────
    // L1: Pure inference — no tools
    // ─────────────────────────────────────────────────────────────────────────
    eprintln!("[L1] Pure inference...");
    results.push(run(
        1,
        "Pure inference",
        "What are the three laws of thermodynamics? List them concisely, one sentence each.",
        Metrics::new(),
        workdir,
        |reply, _| {
            // Model may say "First Law", "Second Law", "Third Law" or "thermodynamics"
            if reply_contains(reply, "entropy").is_ok()
                || reply_contains(reply, "thermodynamic").is_ok()
                || (reply_contains(reply, "First Law").is_ok()
                    && reply_contains(reply, "Second Law").is_ok())
            {
                Ok(())
            } else {
                Err("expected thermodynamics laws content".into())
            }
        },
    ));

    // ─────────────────────────────────────────────────────────────────────────
    // L2a: Single write
    // ─────────────────────────────────────────────────────────────────────────
    eprintln!("[L2a] Single write...");
    results.push(run(
        2,
        "Single WriteFile",
        "Create a file called hello.txt containing exactly: Hello from agnt!",
        Metrics::new(),
        workdir,
        |_, wd| file_contains(wd, "hello.txt", "Hello from agnt!"),
    ));

    // ─────────────────────────────────────────────────────────────────────────
    // L2b: Single read (reads file created by L2a)
    // ─────────────────────────────────────────────────────────────────────────
    eprintln!("[L2b] Single read...");
    results.push(run(
        2,
        "Single ReadFile",
        "Read the file hello.txt and tell me its exact contents.",
        Metrics::new(),
        workdir,
        |reply, _| reply_contains(reply, "Hello from agnt!"),
    ));

    // ─────────────────────────────────────────────────────────────────────────
    // L3: Two-tool chained write+verify
    // ─────────────────────────────────────────────────────────────────────────
    eprintln!("[L3] Two-tool chain...");
    results.push(run(
        3,
        "Write + read-back verify",
        "Create a file called numbers.txt with the numbers 1 through 10, one per line. \
         Then read it back and confirm all 10 numbers are present.",
        Metrics::new(),
        workdir,
        |reply, wd| {
            file_contains(wd, "numbers.txt", "10")?;
            reply_contains(reply, "10")
        },
    ));

    // ─────────────────────────────────────────────────────────────────────────
    // L4: Multi-step arithmetic pipeline
    // ─────────────────────────────────────────────────────────────────────────
    eprintln!("[L4] Multi-step arithmetic pipeline...");
    results.push(run(
        4,
        "Multi-step arithmetic pipeline",
        "Do the following in order:\n\
         1. Create a file called data.txt with these values, one per line: 42, 17, 93, 8, 55\n\
         2. Read data.txt\n\
         3. Compute the sum of all numbers\n\
         4. Write the result to sum.txt as: Sum: <result>\n\
         5. Confirm by reading sum.txt",
        Metrics::new(),
        workdir,
        |reply, wd| {
            file_contains(wd, "data.txt", "93")?;
            file_contains(wd, "sum.txt", "215")?;
            reply_contains(reply, "215")
        },
    ));

    // ─────────────────────────────────────────────────────────────────────────
    // L5: Multi-phase project (3 interdependent files)
    // ─────────────────────────────────────────────────────────────────────────
    eprintln!("[L5] Multi-phase project...");
    results.push(run(
        5,
        "Multi-phase project (3 files)",
        "Build a small project with three files:\n\
         1. config.json — a JSON object with keys: name='agnt-project', version='1.0', debug=true\n\
         2. README.md — a markdown readme that references the project name and version from config.json\n\
         3. manifest.txt — lists both filenames and their sizes in bytes\n\
         After creating all three, read manifest.txt and confirm it lists both config.json and README.md.",
        Metrics::new(),
        workdir,
        |reply, wd| {
            file_contains(wd, "config.json", "agnt-project")?;
            file_contains(wd, "README.md", "agnt-project")?;
            file_contains(wd, "manifest.txt", "config.json")?;
            file_contains(wd, "manifest.txt", "README.md")?;
            reply_contains(reply, "manifest")
        },
    ));

    // ─────────────────────────────────────────────────────────────────────────
    // L6: Context pressure — long prompt + reasoning chain
    // ─────────────────────────────────────────────────────────────────────────
    eprintln!("[L6] Context pressure...");
    let long_prompt = format!(
        "You have been given a complex multi-step task. Work through each step carefully.\n\n\
         Background context (read carefully before proceeding):\n\
         {}\n\n\
         Task:\n\
         1. Summarize the above context in 2 sentences and write it to context_summary.txt\n\
         2. Based on the context, identify 3 key themes and write them to themes.txt, one per line\n\
         3. Write a short analysis.txt that cross-references the themes with the context\n\
         4. Create an index.txt that lists all created files with one-line descriptions\n\
         5. Read and confirm all four files exist and are non-empty",
        "The Rust programming language was first introduced by Mozilla Research in 2010. \
         It was designed with three primary goals: safety, speed, and concurrency. \
         Unlike many systems programming languages, Rust achieves memory safety without \
         a garbage collector by using a concept called ownership with rules that the \
         compiler checks at compile time. Each value in Rust has a variable that is its \
         owner, and there can only be one owner at a time. When the owner goes out of \
         scope, the value is dropped. This ownership model eliminates entire classes of \
         bugs such as null pointer dereferences, dangling pointers, and data races. \
         The borrow checker is the compiler component that enforces these ownership rules. \
         Rust also features a rich type system with generics, traits (similar to interfaces \
         in other languages), and pattern matching. The package ecosystem is managed by \
         Cargo, which also handles building, testing, and dependency management. \
         As of 2024, Rust has been voted the most loved programming language in Stack \
         Overflow's developer survey for nine consecutive years. It is increasingly used \
         in systems programming, WebAssembly, embedded systems, and as a safer alternative \
         to C and C++ in critical infrastructure."
    );
    results.push(run(
        6,
        "Context pressure (long prompt + 4-file chain)",
        &long_prompt,
        Metrics::new(),
        workdir,
        |reply, wd| {
            file_exists(wd, "context_summary.txt")?;
            file_exists(wd, "themes.txt")?;
            file_exists(wd, "analysis.txt")?;
            file_exists(wd, "index.txt")?;
            file_contains(wd, "themes.txt", "\n")?; // at least 2 lines
            reply_contains(reply, "index")
        },
    ));

    // ─────────────────────────────────────────────────────────────────────────
    // L7: Loop stress — task that could tempt repetition
    // ─────────────────────────────────────────────────────────────────────────
    eprintln!("[L7] Loop stress test...");
    results.push(run(
        7,
        "Loop stress (iterative refinement)",
        "Create a file called poem.txt with a 4-line poem about recursion. \
         Then read it back. If any line is shorter than 5 words, rewrite that line \
         and update the file. Repeat until all 4 lines have at least 5 words. \
         Finally, read poem.txt and confirm it has exactly 4 lines all with 5+ words.",
        Metrics::new(),
        workdir,
        |reply, wd| {
            let path = std::path::Path::new(wd).join("poem.txt");
            let content = std::fs::read_to_string(&path)
                .map_err(|e| format!("poem.txt: {}", e))?;
            let lines: Vec<&str> = content.lines().filter(|l| !l.trim().is_empty()).collect();
            if lines.len() < 4 {
                return Err(format!("poem.txt has {} lines, expected 4", lines.len()));
            }
            for (i, line) in lines.iter().enumerate() {
                let words = line.split_whitespace().count();
                if words < 5 {
                    return Err(format!("line {} has only {} words: {:?}", i + 1, words, line));
                }
            }
            Ok(())
        },
    ));

    // ─────────────────────────────────────────────────────────────────────────
    // L8: Degradation probe — large context + coherence check
    // ─────────────────────────────────────────────────────────────────────────
    eprintln!("[L8] Degradation probe (context filling)...");

    // First, fill the agent's context with a few back-and-forth exchanges by
    // reusing the same agent across multiple prompts, then test coherence.
    {
        let backend = agnt::Backend::openai("gemma4-26b", &std::env::var("LITELLM_API_KEY").unwrap_or_else(|_| "none".into()))
            .with_base_url("http://localhost:8001/v1");
        let sandbox = Arc::new(agnt::FilesystemRoot::new(workdir).expect("sandbox"));
        let metrics = Metrics::new();
        let mut agent = Agent::new(
            backend,
            "You are a precise task-execution assistant. Track all state carefully.",
        );
        agent.observer = Arc::clone(&metrics) as Arc<dyn Observer>;
        #[allow(deprecated)]
        {
            agent.stream = false;
        }
        agent
            .tools
            .register(Box::new(ReadFile::with_sandbox(Arc::clone(&sandbox))));
        agent
            .tools
            .register(Box::new(WriteFile::with_sandbox(Arc::clone(&sandbox))));
        agent.max_window = 40;

        let t0 = Instant::now();

        // Warm up: 5 turns to grow the context
        let _ = agent.step("Create warmup_1.txt with the text 'Step 1 complete'");
        let _ = agent.step("Create warmup_2.txt with the text 'Step 2 complete'");
        let _ = agent.step("Create warmup_3.txt with the text 'Step 3 complete'");
        let _ = agent.step("Read warmup_1.txt and tell me what it says");
        let _ = agent.step("Read warmup_2.txt and tell me what it says");

        // The coherence test: can it still follow a precise instruction after context fill?
        let result = agent.step(
            "Write a final file called coherence.txt containing exactly: \
             COHERENCE_CHECK_PASSED. Then read it back and confirm.",
        );

        let elapsed_ms = t0.elapsed().as_millis() as u64;
        let turns = agent.messages.len();
        let (tool_calls, usage) = metrics.drain();

        let (passed, reply_excerpt, failure) = match result {
            Ok(reply) => {
                let check = file_contains(workdir, "coherence.txt", "COHERENCE_CHECK_PASSED");
                let excerpt = reply.chars().take(300).collect::<String>();
                match check {
                    Ok(()) => (true, excerpt, None),
                    Err(e) => (false, excerpt, Some(format!("coherence check: {}", e))),
                }
            }
            Err(e) => (false, String::new(), Some(e)),
        };

        results.push(ScenarioResult {
            name: "Degradation probe (6-turn context + coherence)".into(),
            level: 8,
            passed,
            elapsed_ms,
            turns,
            tool_calls,
            usage,
            reply_excerpt,
            failure,
        });
    }

    eprintln!("\n=== eval complete — generating report ===\n");
    print_report(&results);
}
