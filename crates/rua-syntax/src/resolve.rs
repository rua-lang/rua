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
use std::collections::HashSet;
use std::rc::Rc;

/// The most locals one function can have: slots are `u16`.
pub const MAX_SLOTS: usize = u16::MAX as usize - 256;

/// Resolve a chunk, returning it and the number of frame slots it needs.
pub fn resolve_chunk(block: &Block) -> (Block, usize) {
    let mut r = Resolver { scopes: vec![FuncScope::new(captured_names(block))] };
    let out = r.block(block);
    (out, r.scopes[0].n_slots)
}

// ---- pre-pass: which names do nested closures capture? ---------------------

fn captured_names(body: &Block) -> HashSet<Rc<str>> {
    let mut out = HashSet::new();
    scan_block(body, &mut out, false);
    out
}

fn scan_block(b: &Block, out: &mut HashSet<Rc<str>>, inside: bool) {
    for st in &b.stats {
        scan_stat(st, out, inside);
    }
    if let Some(t) = &b.tail {
        scan_expr(t, out, inside);
    }
}

fn scan_stat(st: &Stat, out: &mut HashSet<Rc<str>>, inside: bool) {
    match st {
        Stat::Let(_, exprs) | Stat::LetSlots(_, exprs) | Stat::Return(exprs) => {
            exprs.iter().for_each(|e| scan_expr(e, out, inside))
        }
        Stat::FnDecl(_, e) | Stat::FnSlot(_, e) => scan_expr(e, out, inside),
        Stat::Assign(targets, exprs) => {
            targets.iter().chain(exprs).for_each(|e| scan_expr(e, out, inside))
        }
        Stat::OpAssign(t, _, e) => {
            scan_expr(t, out, inside);
            scan_expr(e, out, inside);
        }
        Stat::Expr(e) => scan_expr(e, out, inside),
        Stat::While(_, c, b) => {
            scan_expr(c, out, inside);
            scan_block(b, out, inside);
        }
        Stat::Loop(_, b) => scan_block(b, out, inside),
        Stat::ForRange { start, end, body, .. } => {
            scan_expr(start, out, inside);
            scan_expr(end, out, inside);
            scan_block(body, out, inside);
        }
        Stat::ForIn { iter, body, .. } => {
            scan_expr(iter, out, inside);
            scan_block(body, out, inside);
        }
        Stat::Break | Stat::Continue => {}
    }
}

fn scan_expr(e: &Expr, out: &mut HashSet<Rc<str>>, inside: bool) {
    match e {
        // a name read inside a nested function is a capture candidate
        Expr::Var(n) if inside => {
            out.insert(n.clone());
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
            scan_expr(a, out, inside);
            scan_expr(b, out, inside);
        }
        Expr::Un(_, a) => scan_expr(a, out, inside),
        Expr::Call(f, args) => {
            scan_expr(f, out, inside);
            args.iter().for_each(|a| scan_expr(a, out, inside));
        }
        Expr::Method(o, _, args) => {
            scan_expr(o, out, inside);
            args.iter().for_each(|a| scan_expr(a, out, inside));
        }
        // everything a nested function reads counts as a capture
        Expr::Func(def) => scan_block(&def.body, out, true),
        Expr::Array(items) => items.iter().for_each(|i| scan_expr(i, out, inside)),
        Expr::Map(items) => items.iter().for_each(|(k, v)| {
            scan_expr(k, out, inside);
            scan_expr(v, out, inside);
        }),
        Expr::If(arms, els) => {
            for (c, b) in arms {
                scan_expr(c, out, inside);
                scan_block(b, out, inside);
            }
            if let Some(b) = els {
                scan_block(b, out, inside);
            }
        }
        Expr::Match(subject, arms) => {
            scan_expr(subject, out, inside);
            for arm in arms {
                for p in &arm.patterns {
                    if let Pattern::Lit(e) = p {
                        scan_expr(e, out, inside);
                    }
                }
                if let Some(g) = &arm.guard {
                    scan_expr(g, out, inside);
                }
                scan_block(&arm.body, out, inside);
            }
        }
        Expr::Do(b) => scan_block(b, out, inside),
    }
}

// ---- the resolver ---------------------------------------------------------

struct Local {
    name: Rc<str>,
    binding: Binding,
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
        self.locals.iter().rev().find(|l| &*l.name == name).map(|l| l.binding)
    }
}

/// The stack of functions currently being resolved; the last one is current.
struct Resolver {
    scopes: Vec<FuncScope>,
}

impl Resolver {
    /// Are we in the outermost block of the chunk itself?
    fn at_chunk_top(&self) -> bool {
        self.scopes.len() == 1 && self.scopes[0].marks.len() == 1
    }

    fn cur(&mut self) -> &mut FuncScope {
        self.scopes.last_mut().expect("a scope is always open")
    }

    fn declare(&mut self, name: &Rc<str>) -> Binding {
        let s = self.cur();
        assert!(s.next_slot < MAX_SLOTS, "a function may hold at most {MAX_SLOTS} locals");
        let binding = Binding { slot: s.next_slot as u16, cell: s.captured.contains(name) };
        s.next_slot += 1;
        s.n_slots = s.n_slots.max(s.next_slot);
        s.locals.push(Local { name: name.clone(), binding });
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

    fn var(&mut self, name: &Rc<str>) -> Expr {
        let top = self.scopes.len() - 1;
        if let Some(b) = self.scopes[top].lookup(name) {
            return Expr::Local(b, name.clone());
        }
        match self.capture(top, name) {
            Some(i) => Expr::Upval(i, name.clone()),
            None => Expr::Global(name.clone(), GlobalCache::new()),
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
                    let f = self.expr(f);
                    Stat::Assign(vec![Expr::Global(name.clone(), GlobalCache::new())], vec![f])
                } else {
                    // bound first, so the body can refer to itself
                    let binding = self.declare(name);
                    Stat::FnSlot(binding, self.expr(f))
                }
            }
            Stat::FnSlot(b, f) => Stat::FnSlot(*b, self.expr(f)),
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
