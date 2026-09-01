//! The `rua` command: run a script, evaluate `-e` chunks, or start a REPL.

use clap::Parser;
use miette::{Diagnostic, NamedSource, SourceSpan};
use rua::{Value, Vm};
use std::io::Write;

/// rua allocates a small object per table and frees it again; the general
/// purpose allocator's small-size path is a measurable share of any program
/// that builds data structures, and this one's is much shorter.
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[derive(Parser)]
#[command(
    name = "rua",
    version,
    about = "A small Rust-shaped scripting language with a rustc-backed JIT",
    after_help = "With no script and no -e, rua starts a REPL."
)]
struct Args {
    /// Script to run
    script: Option<String>,

    /// Arguments passed to the script, readable there as `arg`
    #[arg(trailing_var_arg = true)]
    script_args: Vec<String>,

    /// Evaluate a chunk of source (repeatable)
    #[arg(short = 'e', long = "eval", value_name = "CHUNK")]
    eval: Vec<String>,

    /// Start the REPL after running the script
    #[arg(short, long)]
    interactive: bool,

    /// Interpret everything: never call rustc
    #[arg(long)]
    no_jit: bool,

    /// Compile a function after this many calls
    #[arg(long, value_name = "N", default_value_t = 50)]
    jit: u32,

    /// Print the Rust the JIT generates
    #[arg(long)]
    dump_jit: bool,

    /// Print the bytecode the compiler generates, and run nothing
    #[arg(long)]
    dump_bytecode: bool,
}

/// A rua error, dressed up for miette: the message, the line it happened on,
/// and the call stack that led there.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
struct Report {
    message: String,
    src: NamedSource<String>,
    span: Option<SourceSpan>,
    label: String,
    trace: Vec<String>,
}

impl Diagnostic for Report {
    fn source_code(&self) -> Option<&dyn miette::SourceCode> {
        Some(&self.src)
    }

    fn labels(&self) -> Option<Box<dyn Iterator<Item = miette::LabeledSpan> + '_>> {
        let span = self.span?;
        Some(Box::new(std::iter::once(miette::LabeledSpan::new_with_span(
            Some(self.label.clone()),
            span,
        ))))
    }

    fn help<'a>(&'a self) -> Option<Box<dyn std::fmt::Display + 'a>> {
        if self.trace.is_empty() {
            return None;
        }
        Some(Box::new(self.trace.join("\n")))
    }
}

/// The byte range of one 1-based line, for miette to underline.
fn line_span(src: &str, line: u32) -> Option<SourceSpan> {
    if line == 0 {
        return None;
    }
    let mut offset = 0usize;
    for (i, text) in src.lines().enumerate() {
        if i as u32 + 1 == line {
            let trimmed = text.trim_end();
            return Some(SourceSpan::new(offset.into(), trimmed.len().max(1)));
        }
        offset += text.len() + 1;
    }
    None
}

fn report(e: &rua::Error, src: &str, name: &str) -> Report {
    Report {
        message: e.message.clone(),
        src: NamedSource::new(name, src.to_string()).with_language("rust"),
        span: if e.located { line_span(src, e.line) } else { None },
        label: match &e.where_ {
            Some(f) => format!("in {f}"),
            None => "here".to_string(),
        },
        trace: e.traceback(),
    }
}

fn main() {
    let args = Args::parse();
    let mut vm = Vm::new();
    vm.jit.enabled = !args.no_jit;
    vm.jit.threshold = args.jit.max(1);
    // the flag turns dumping on; RUA_JIT_DUMP may already have
    vm.jit.dump = vm.jit.dump || args.dump_jit;

    // script arguments, as an array
    let arg_table = std::rc::Rc::new(std::cell::RefCell::new(rua::Table::new()));
    for a in &args.script_args {
        arg_table.borrow_mut().push(Value::str(a));
    }
    vm.set_global("arg", Value::Table(arg_table));

    if args.dump_bytecode {
        let sources = args.eval.iter().cloned().chain(
            args.script
                .as_ref()
                .and_then(|p| std::fs::read_to_string(p).ok()),
        );
        for src in sources {
            match vm.dump_bytecode(&src) {
                Ok(text) => print!("{text}"),
                Err(e) => {
                    eprintln!("rua: {e}");
                    std::process::exit(1);
                }
            }
        }
        return;
    }

    for chunk in &args.eval {
        run(&mut vm, chunk, "<argument>");
    }
    if let Some(path) = &args.script {
        match std::fs::read_to_string(path) {
            Ok(src) => run(&mut vm, &src, path),
            Err(e) => {
                eprintln!("rua: cannot read {path}: {e}");
                std::process::exit(1);
            }
        }
    }
    if args.interactive || (args.script.is_none() && args.eval.is_empty()) {
        run_repl(&mut vm);
    }
}

fn run(vm: &mut Vm, src: &str, name: &str) {
    if let Err(e) = vm.eval(src) {
        eprintln!("{:?}", miette::Report::new(report(&e, src, name)));
        std::process::exit(1);
    }
}

/// In the REPL a top level `let x = ..` becomes `x = ..`, a global, so that the
/// binding is still there on the next line. Anything else is left alone.
fn repl_rewrite(line: &str) -> String {
    let rest = match line.strip_prefix("let ") {
        Some(r) => r.trim_start().strip_prefix("mut ").unwrap_or_else(|| r.trim_start()),
        None => return line.to_string(),
    };
    let name: String = rest.chars().take_while(|c| c.is_alphanumeric() || *c == '_').collect();
    let after = rest[name.len()..].trim_start();
    if !name.is_empty() && after.starts_with('=') && !after.starts_with("==") {
        format!("{name} {after}")
    } else {
        line.to_string()
    }
}

fn run_repl(vm: &mut Vm) {
    println!(
        "rua {}  (jit: {})  ^D to exit",
        env!("CARGO_PKG_VERSION"),
        if vm.jit.enabled { "on" } else { "off" }
    );
    println!("top level `let` binds a global here, so it survives the next line");
    let stdin = std::io::stdin();
    loop {
        print!("> ");
        let _ = std::io::stdout().flush();
        let mut line = String::new();
        match stdin.read_line(&mut line) {
            Ok(0) => {
                println!();
                return;
            }
            Ok(_) => {}
            Err(e) => {
                eprintln!("rua: {e}");
                return;
            }
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        // a chunk's value is its final expression, so `1 + 1` prints 2
        let source = repl_rewrite(trimmed);
        match vm.eval(&source) {
            Ok(vals) if !vals.is_empty() => {
                let out: Vec<String> = vals.iter().map(|v| v.to_string()).collect();
                println!("{}", out.join(" "));
            }
            Ok(_) => {}
            Err(e) => {
                let r = miette::Report::new(report(&e, &source, "<repl>"));
                println!("{r:?}");
            }
        }
    }
}
