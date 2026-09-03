//! Closure conversion: turn names into frame slots before anything runs.
//!
//! A tree-walker that looks a variable up in a hash map on every access spends
//! most of its time hashing. This pass gives every local a slot in the call
//! frame, every captured variable an upvalue index, and leaves only real
//! globals to be looked up by name.
//!
//! A local that a nested closure captures lives in a cell (`Rc<RefCell<Value>>`)
//! so both sides share it; every other local is a plain value in the frame.

use crate::ast::*;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// The most locals one function can have: slots are `u16`.
pub const MAX_SLOTS: usize = u16::MAX as usize - 256;

/// Resolve a chunk, returning it and the number of frame slots it needs.
pub fn resolve_chunk(block: &Block) -> (Block, usize) {
    let mut r = Resolver::new(block, false);
    let out = r.block(block);
    (out, r.scopes[0].n_slots)
}

/// Resolve a chunk for the sake of the answer rather than the tree: every
/// name it mentions, and what each one turned out to refer to.
pub fn occurrences(block: &Block) -> Vec<Occurrence> {
    let mut r = Resolver::new(block, true);
    let _ = r.block(block);
    r.seen
}

// ---- pre-pass: which names do nested closures capture? ---------------------

fn captured_names(body: &Block) -> HashSet<Rc<str>> {
    let mut out = HashSet::new();
    let mut scan = Scan { out: &mut out, scopes: Vec::new() };
    scan.block(body, false);
    out
}

/// The walk that decides which names a nested function reads from outside
/// itself.
///
/// It has to know what the nested function binds for itself, or a parameter
/// named the same as an outer local marks that local as captured — which puts
/// it in a heap cell, slows every read of it, and stops the compiler taking
/// any loop that touches it. `fn advance(bodies, dt)` beside a `let bodies`
/// was exactly that.
struct Scan<'a> {
    out: &'a mut HashSet<Rc<str>>,
    /// Names bound inside the nested function, innermost scope last.
    scopes: Vec<HashSet<Rc<str>>>,
}

impl Scan<'_> {
    fn bound(&self, name: &Rc<str>) -> bool {
        self.scopes.iter().any(|s| s.contains(name))
    }

    fn declare(&mut self, name: &Rc<str>) {
        if let Some(s) = self.scopes.last_mut() {
            s.insert(name.clone());
        }
    }

    fn block(&mut self, b: &Block, inside: bool) {
        if inside {
            self.scopes.push(HashSet::new());
        }
        for st in &b.stats {
            self.stat(st, inside);
        }
        if let Some(t) = &b.tail {
            self.expr(t, inside);
        }
        if inside {
            self.scopes.pop();
        }
    }

    fn stat(&mut self, st: &Stat, inside: bool) {
        match st {
            // the values are read first; the names come into scope after
            Stat::Let(names, exprs) => {
                exprs.iter().for_each(|e| self.expr(e, inside));
                for n in names {
                    self.declare(&n.text);
                }
            }
            Stat::LetSlots(_, exprs) | Stat::Return(exprs) => {
                exprs.iter().for_each(|e| self.expr(e, inside))
            }
            // a type declares nothing that runs, so there is nothing here to
            // capture and nothing to declare
            Stat::TypeAlias(..) => {}
            Stat::Impl(_, methods) => {
                methods.iter().for_each(|(_, f)| self.expr(f, inside));
            }
            Stat::FnDecl(name, e) => {
                self.declare(&name.text);
                self.expr(e, inside);
            }
            Stat::FnSlot(_, e) => self.expr(e, inside),
            Stat::Assign(targets, exprs) => {
                targets.iter().chain(exprs).for_each(|e| self.expr(e, inside))
            }
            Stat::OpAssign(t, _, e) => {
                self.expr(t, inside);
                self.expr(e, inside);
            }
            Stat::Expr(e) => self.expr(e, inside),
            Stat::While(_, c, b) => {
                self.expr(c, inside);
                self.block(b, inside);
            }
            Stat::Loop(_, b) => self.block(b, inside),
            Stat::ForRange { var, start, end, body, .. } => {
                self.expr(start, inside);
                self.expr(end, inside);
                if inside {
                    self.scopes.push(HashSet::from([var.text.clone()]));
                }
                self.block(body, inside);
                if inside {
                    self.scopes.pop();
                }
            }
            Stat::ForIn { vars, iter, body, .. } => {
                self.expr(iter, inside);
                if inside {
                    self.scopes.push(vars.iter().map(|v| v.text.clone()).collect());
                }
                self.block(body, inside);
                if inside {
                    self.scopes.pop();
                }
            }
            Stat::Break | Stat::Continue => {}
        }
    }

    fn expr(&mut self, e: &Expr, inside: bool) {
        match e {
            // a name read inside a nested function, and not bound by it, is
            // read from the enclosing frame
            Expr::Var(n) if inside && !self.bound(&n.text) => {
                self.out.insert(n.text.clone());
            }
            Expr::Var(_)
            | Expr::Nil
            | Expr::Bool(_)
            | Expr::Num(_)
            | Expr::Str(_)
            | Expr::Local(..)
            | Expr::Upval(..)
            | Expr::Global(..) => {}
            Expr::Index(a, b) | Expr::Bin(_, a, b) | Expr::Range(a, b, _) => {
                self.expr(a, inside);
                self.expr(b, inside);
            }
            Expr::Un(_, a) => self.expr(a, inside),
            Expr::Call(f, args) => {
                self.expr(f, inside);
                args.iter().for_each(|a| self.expr(a, inside));
            }
            Expr::Method(o, _, args) => {
                self.expr(o, inside);
                args.iter().for_each(|a| self.expr(a, inside));
            }
            // everything a nested function reads from outside itself counts
            Expr::Func(def) => {
                self.scopes.push(def.params.iter().map(|p| p.text.clone()).collect());
                self.block(&def.body, true);
                self.scopes.pop();
            }
            Expr::Array(items) => items.iter().for_each(|i| self.expr(i, inside)),
            Expr::Map(items) => items.iter().for_each(|(k, v)| {
                self.expr(k, inside);
                self.expr(v, inside);
            }),
            Expr::If(arms, els) => {
                for (c, b) in arms {
                    self.expr(c, inside);
                    self.block(b, inside);
                }
                if let Some(b) = els {
                    self.block(b, inside);
                }
            }
            Expr::Match(subject, arms) => {
                self.expr(subject, inside);
                for arm in arms {
                    for p in &arm.patterns {
                        if let Pattern::Lit(e) = p {
                            self.expr(e, inside);
                        }
                    }
                    self.block(&arm.body, inside);
                }
            }
            Expr::Do(b) => self.block(b, inside),
        }
    }
}

/// A type, as a value that describes it.
///
/// The shapes are ordinary tables, so nothing new is needed to build them,
/// pass them around or read them: `#{ kind: "record", fields: #{ .. } }`. A
/// name inside one stays a name and is followed when the check runs, so that
/// two types may refer to each other.
fn describe(t: &Type) -> Expr {
    fn field(k: &str, v: Expr) -> (Expr, Expr) {
        (Expr::Str(k.into()), v)
    }
    fn word(s: &str) -> Expr {
        Expr::Str(s.into())
    }
    match t {
        Type::Named(n, args, _) => {
            let known = matches!(
                &**n,
                "number" | "string" | "boolean" | "nil" | "table" | "function" | "any"
            );
            if known && args.is_empty() {
                return Expr::Map(vec![field("kind", word(n))]);
            }
            let mut entries = vec![field("kind", word("name")), field("name", word(n))];
            if !args.is_empty() {
                entries.push(field("args", Expr::Array(args.iter().map(describe).collect())));
            }
            Expr::Map(entries)
        }
        Type::Array(inner, _) => Expr::Map(vec![
            field("kind", word("array")),
            field("of", describe(inner)),
        ]),
        Type::Record(fields, _) => Expr::Map(vec![
            field("kind", word("record")),
            field(
                "fields",
                Expr::Map(
                    fields
                        .iter()
                        .map(|(n, ft)| (Expr::Str(n.text.clone()), describe(ft)))
                        .collect(),
                ),
            ),
        ]),
        // what a function takes cannot be seen from the outside, so a check
        // can only ask whether it is one
        Type::Fn(..) => Expr::Map(vec![field("kind", word("function"))]),
    }
}

// ---- what the resolver saw ------------------------------------------------

/// One place a name is written, and what it turned out to mean.
///
/// The resolver already decides this — which `x` an `x` refers to, through
/// which scopes, past which shadowing. Writing it down as it goes is what
/// lets an editor answer the same question without a second opinion about
/// how rua's scopes work.
#[derive(Debug, Clone)]
pub struct Occurrence {
    /// The bytes of this mention of the name.
    pub span: Span,
    pub name: Rc<str>,
    /// Where the name came into scope, when it did so in this file. A global
    /// has none: it may have been defined anywhere, or nowhere yet.
    pub decl: Option<Span>,
    /// Is this the declaration itself?
    pub is_decl: bool,
    pub kind: RefKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefKind {
    Local,
    /// A local of an enclosing function, reached through a closure.
    Upvalue,
    Global,
}

// ---- the resolver ---------------------------------------------------------

struct Local {
    name: Rc<str>,
    binding: Binding,
    /// Where the name was written when it came into scope, which is what
    /// go-to-definition answers with.
    decl: Span,
}

struct FuncScope {
    locals: Vec<Local>,
    marks: Vec<(usize, usize)>,
    n_slots: usize,
    next_slot: usize,
    captured: HashSet<Rc<str>>,
    upvals: Vec<UpvalSrc>,
    upval_names: Vec<Rc<str>>,
}

impl FuncScope {
    fn new(captured: HashSet<Rc<str>>) -> Self {
        FuncScope {
            locals: Vec::new(),
            marks: Vec::new(),
            n_slots: 0,
            next_slot: 0,
            captured,
            upvals: Vec::new(),
            upval_names: Vec::new(),
        }
    }

    fn lookup(&self, name: &str) -> Option<Binding> {
        self.find(name).map(|l| l.binding)
    }

    /// The innermost local of that name, which is the one a mention means.
    fn find(&self, name: &str) -> Option<&Local> {
        self.locals.iter().rev().find(|l| &*l.name == name)
    }
}

/// The stack of functions currently being resolved; the last one is current.
struct Resolver {
    scopes: Vec<FuncScope>,
    /// Every name written, with what it referred to. Only filled in when
    /// somebody asked: the interpreter never looks at it.
    seen: Vec<Occurrence>,
    watching: bool,
    /// Top level `fn`s, which are globals rather than locals, and where each
    /// was written. Collected before the walk so that a call above the
    /// function it calls still finds it.
    top_fns: HashMap<Rc<str>, Span>,
}

impl Resolver {
    fn new(block: &Block, watching: bool) -> Resolver {
        let mut top_fns = HashMap::new();
        if watching {
            for st in &block.stats {
                if let Stat::FnDecl(name, _) = st {
                    top_fns.insert(name.text.clone(), name.span);
                }
            }
        }
        Resolver {
            scopes: vec![FuncScope::new(captured_names(block))],
            seen: Vec::new(),
            watching,
            top_fns,
        }
    }

    /// Are we in the outermost block of the chunk itself?
    fn at_chunk_top(&self) -> bool {
        self.scopes.len() == 1 && self.scopes[0].marks.len() == 1
    }

    fn cur(&mut self) -> &mut FuncScope {
        self.scopes.last_mut().expect("a scope is always open")
    }

    fn declare(&mut self, name: &Name) -> Binding {
        let s = self.cur();
        assert!(s.next_slot < MAX_SLOTS, "a function may hold at most {MAX_SLOTS} locals");
        let binding = Binding { slot: s.next_slot as u16, cell: s.captured.contains(&name.text) };
        s.next_slot += 1;
        s.n_slots = s.n_slots.max(s.next_slot);
        s.locals.push(Local { name: name.text.clone(), binding, decl: name.span });
        self.note(name, Some(name.span), true, RefKind::Local);
        binding
    }

    fn open(&mut self) {
        let s = self.cur();
        let mark = (s.locals.len(), s.next_slot);
        s.marks.push(mark);
    }

    fn close(&mut self) {
        let s = self.cur();
        if let Some((locals, slot)) = s.marks.pop() {
            s.locals.truncate(locals);
            s.next_slot = slot; // slots are reused once the block ends
        }
    }

    /// Thread `name` down from the function that owns it, adding an upvalue
    /// entry at every level in between.
    fn capture(&mut self, level: usize, name: &Rc<str>) -> Option<u16> {
        if level == 0 {
            return None;
        }
        if let Some(i) = self.scopes[level].upval_names.iter().position(|n| n == name) {
            return Some(i as u16);
        }
        let src = match self.scopes[level - 1].lookup(name) {
            Some(b) => UpvalSrc::ParentLocal(b.slot),
            None => UpvalSrc::ParentUpval(self.capture(level - 1, name)?),
        };
        let s = &mut self.scopes[level];
        s.upvals.push(src);
        s.upval_names.push(name.clone());
        Some((s.upvals.len() - 1) as u16)
    }

    fn var(&mut self, name: &Name) -> Expr {
        let top = self.scopes.len() - 1;
        if let Some(l) = self.scopes[top].find(name) {
            let (binding, decl) = (l.binding, l.decl);
            self.note(name, Some(decl), false, RefKind::Local);
            return Expr::Local(binding, name.text.clone());
        }
        // a local of an enclosing function: the declaration is over there,
        // and the scope that owns it still knows where
        let outer = self.declared_outside(&name.text);
        match self.capture(top, &name.text) {
            Some(i) => {
                self.note(name, outer, false, RefKind::Upvalue);
                Expr::Upval(i, name.text.clone())
            }
            None => {
                // a `fn` at the top of this file is a global, and this file
                // is where it was written
                let decl = self.top_fns.get(&name.text).copied();
                self.note(name, decl, false, RefKind::Global);
                Expr::Global(name.text.clone(), GlobalCache::new())
            }
        }
    }

    /// Where an enclosing function declared this name, if one did.
    fn declared_outside(&self, name: &str) -> Option<Span> {
        self.scopes[..self.scopes.len() - 1]
            .iter()
            .rev()
            .find_map(|s| s.locals.iter().rev().find(|l| &*l.name == name).map(|l| l.decl))
    }

    /// Write down one mention of a name.
    fn note(&mut self, name: &Name, decl: Option<Span>, is_decl: bool, kind: RefKind) {
        if self.watching {
            self.seen.push(Occurrence {
                span: name.span,
                name: name.text.clone(),
                decl,
                is_decl,
                kind,
            });
        }
    }

    fn block(&mut self, b: &Block) -> Block {
        self.open();
        let out = self.block_inner(b);
        self.close();
        out
    }

    /// A block that shares the surrounding scope — a function body, whose
    /// parameters are already declared.
    fn block_inner(&mut self, b: &Block) -> Block {
        let stats = b.stats.iter().map(|s| self.stat(s)).collect();
        let tail = b.tail.as_ref().map(|t| Box::new(self.expr(t)));
        Block { stats, lines: b.lines.clone(), tail, tail_line: b.tail_line }
    }

    fn stat(&mut self, st: &Stat) -> Stat {
        match st {
            Stat::Let(names, exprs) => {
                // the values are evaluated before the names come into scope
                let exprs: Vec<Expr> = exprs.iter().map(|e| self.expr(e)).collect();
                let bindings = names.iter().map(|n| self.declare(n)).collect();
                Stat::LetSlots(bindings, exprs)
            }
            Stat::LetSlots(b, exprs) => {
                Stat::LetSlots(b.clone(), exprs.iter().map(|e| self.expr(e)).collect())
            }
            Stat::FnDecl(name, f) => {
                if self.at_chunk_top() {
                    // top level `fn` is a global, the way a Rust item belongs
                    // to its module: that is what embedders reach for by name
                    self.note(name, Some(name.span), true, RefKind::Global);
                    let f = self.expr(f);
                    Stat::Assign(
                        vec![Expr::Global(name.text.clone(), GlobalCache::new())],
                        vec![f],
                    )
                } else {
                    // bound first, so the body can refer to itself
                    let binding = self.declare(name);
                    Stat::FnSlot(binding, self.expr(f))
                }
            }
            Stat::FnSlot(b, f) => Stat::FnSlot(*b, self.expr(f)),
            // A type becomes a value describing itself, bound to its own
            // name. That is what lets the same declaration guard a marshall
            // at run time as well as answer the editor — one shape written
            // once, rather than a type and a validator that drift apart.
            // `impl Vec2 { fn len(self) .. }` is `Vec2.len = fn .. `, which
            // needs nothing new to run and is what anybody may write by hand
            Stat::Impl(name, methods) => {
                let (mut targets, mut values) = (Vec::new(), Vec::new());
                for (m, f) in methods {
                    targets.push(Expr::Index(
                        Box::new(Expr::Global(name.text.clone(), GlobalCache::new())),
                        Box::new(Expr::Str(m.text.clone())),
                    ));
                    values.push(self.expr(f));
                }
                Stat::Assign(targets, values)
            }
            Stat::TypeAlias(name, params, t) => {
                if params.is_empty() {
                    Stat::Assign(
                        vec![Expr::Global(name.text.clone(), GlobalCache::new())],
                        vec![describe(t)],
                    )
                } else {
                    // one that takes parameters describes nothing on its own;
                    // it is a shape with holes until they are filled
                    Stat::TypeAlias(name.clone(), params.clone(), t.clone())
                }
            }
            Stat::Assign(targets, exprs) => Stat::Assign(
                targets.iter().map(|t| self.expr(t)).collect(),
                exprs.iter().map(|e| self.expr(e)).collect(),
            ),
            Stat::OpAssign(t, op, e) => Stat::OpAssign(self.expr(t), *op, self.expr(e)),
            Stat::Expr(e) => Stat::Expr(self.expr(e)),
            Stat::While(id, c, b) => Stat::While(*id, self.expr(c), self.block(b)),
            Stat::Loop(id, b) => Stat::Loop(*id, self.block(b)),
            Stat::ForRange { id, var, start, end, inclusive, body, .. } => {
                let start = self.expr(start);
                let end = self.expr(end);
                self.open();
                let binding = Some(self.declare(var));
                let body = self.block_inner(body);
                self.close();
                Stat::ForRange {
                    id: *id,
                    var: var.clone(),
                    binding,
                    start,
                    end,
                    inclusive: *inclusive,
                    body,
                }
            }
            Stat::ForIn { id, vars, iter, body, .. } => {
                let iter = self.expr(iter);
                self.open();
                let bindings = vars.iter().map(|v| self.declare(v)).collect();
                let body = self.block_inner(body);
                self.close();
                Stat::ForIn { id: *id, vars: vars.clone(), bindings, iter, body }
            }
            Stat::Return(exprs) => Stat::Return(exprs.iter().map(|e| self.expr(e)).collect()),
            Stat::Break => Stat::Break,
            Stat::Continue => Stat::Continue,
        }
    }

    fn expr(&mut self, e: &Expr) -> Expr {
        match e {
            Expr::Var(n) => self.var(n),
            Expr::Nil
            | Expr::Bool(_)
            | Expr::Num(_)
            | Expr::Str(_)
            | Expr::Local(..)
            | Expr::Upval(..)
            | Expr::Global(..) => e.clone(),
            Expr::Index(a, b) => Expr::Index(Box::new(self.expr(a)), Box::new(self.expr(b))),
            Expr::Bin(op, a, b) => Expr::Bin(*op, Box::new(self.expr(a)), Box::new(self.expr(b))),
            Expr::Un(op, a) => Expr::Un(*op, Box::new(self.expr(a))),
            Expr::Range(a, b, inc) => {
                Expr::Range(Box::new(self.expr(a)), Box::new(self.expr(b)), *inc)
            }
            Expr::Call(f, args) => Expr::Call(
                Box::new(self.expr(f)),
                args.iter().map(|a| self.expr(a)).collect(),
            ),
            Expr::Method(o, name, args) => Expr::Method(
                Box::new(self.expr(o)),
                name.clone(),
                args.iter().map(|a| self.expr(a)).collect(),
            ),
            Expr::Array(items) => Expr::Array(items.iter().map(|i| self.expr(i)).collect()),
            Expr::Map(items) => {
                Expr::Map(items.iter().map(|(k, v)| (self.expr(k), self.expr(v))).collect())
            }
            Expr::If(arms, els) => Expr::If(
                arms.iter().map(|(c, b)| (self.expr(c), self.block(b))).collect(),
                els.as_ref().map(|b| self.block(b)),
            ),
            Expr::Match(subject, arms) => {
                let subject = Box::new(self.expr(subject));
                let arms = arms
                    .iter()
                    .map(|arm| {
                        // each arm is its own scope: a pattern name binds there
                        self.open();
                        let patterns = arm
                            .patterns
                            .iter()
                            .map(|p| match p {
                                Pattern::Bind(name, _) => {
                                    Pattern::Bind(name.clone(), Some(self.declare(name)))
                                }
                                Pattern::Lit(e) => Pattern::Lit(self.expr(e)),
                                Pattern::Wild => Pattern::Wild,
                            })
                            .collect();
                        let guard = arm.guard.as_ref().map(|g| self.expr(g));
                        let body = self.block_inner(&arm.body);
                        self.close();
                        Arm { patterns, guard, body }
                    })
                    .collect();
                Expr::Match(subject, arms)
            }
            Expr::Do(b) => Expr::Do(self.block(b)),
            Expr::Func(def) => {
                self.scopes.push(FuncScope::new(captured_names(&def.body)));
                let param_bindings: Vec<Binding> =
                    def.params.iter().map(|p| self.declare(p)).collect();
                let body = self.block_inner(&def.body);
                let scope = self.scopes.pop().expect("we pushed one");
                Expr::Func(Rc::new(FuncDef {
                    id: def.id,
                    ret: def.ret.clone(),
                    name: def.name.clone(),
                    params: def.params.clone(),
                    body,
                    line: def.line,
                    n_slots: scope.n_slots,
                    param_bindings,
                    upvals: scope.upvals,
                }))
            }
        }
    }
}
