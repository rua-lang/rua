//! Turn `v.len()` into `Vec2::len(v)` where the receiver's type is known.
//!
//! `impl` puts a shape's methods on the shape's own table, so the call this
//! produces is one anybody could have written by hand — and costs exactly
//! what writing it by hand costs. Nothing is added to a value at run time:
//! no prototype pointer, no method carried on every instance, no lookup
//! through a chain. A method resolves the way Rust's does, statically, and
//! rua can do that here because the annotation says which shape it is.
//!
//! What it does not know, it leaves alone: a receiver with no type keeps its
//! ordinary dispatch, which looks for a field and then asks the runtime.

use crate::ast::{Block, Expr, GlobalCache, Name, Stat, Type};
use std::collections::HashMap;
use std::rc::Rc;

/// Rewrite the method calls whose receiver is known.
pub fn lower(block: &Block) -> Block {
    let mut cx = Lower::default();
    cx.collect(block);
    if cx.methods.is_empty() {
        // nothing was implemented, so nothing can be resolved
        return block.clone();
    }
    cx.scopes.push(HashMap::new());
    cx.block(block)
}

#[derive(Default)]
struct Lower {
    /// `type Name = T`, for following one name to another.
    aliases: HashMap<Rc<str>, Type>,
    /// Which shapes have which methods, and what each hands back — so that
    /// `v.scaled(2).len()` finds the second method as well as the first.
    methods: HashMap<Rc<str>, Vec<(Rc<str>, Option<Type>)>>,
    /// What a function hands back, when it says.
    returns: HashMap<Rc<str>, Rc<str>>,
    /// Names in scope, and the shape each one is.
    scopes: Vec<HashMap<Rc<str>, Rc<str>>>,
}

impl Lower {
    fn collect(&mut self, b: &Block) {
        for s in &b.stats {
            // a type and its methods may be written inside a function, and
            // are found the same way there
            match s {
                Stat::While(_, _, body) | Stat::Loop(_, body) => self.collect(body),
                Stat::ForRange { body, .. } | Stat::ForIn { body, .. } => self.collect(body),
                Stat::FnDecl(_, Expr::Func(def)) => self.collect(&def.body),
                Stat::Expr(e) | Stat::FnSlot(_, e) => self.collect_expr(e),
                Stat::Let(_, es) | Stat::Return(es) => {
                    es.iter().for_each(|e| self.collect_expr(e))
                }
                _ => {}
            }
            match s {
                Stat::TypeAlias(name, _, t) => {
                    self.aliases.insert(name.text.clone(), t.clone());
                }
                Stat::Impl(name, _, ms) => {
                    let entry = self.methods.entry(name.text.clone()).or_default();
                    for (m, f) in ms {
                        let ret = match f {
                            Expr::Func(def) => def.ret.clone(),
                            _ => None,
                        };
                        entry.push((m.text.clone(), ret));
                    }
                }
                Stat::FnDecl(name, Expr::Func(def)) => {
                    if let Some(shape) = def.ret.as_ref().and_then(|r| self.shape_of(r)) {
                        self.returns.insert(name.text.clone(), shape);
                    }
                }
                _ => {}
            }
        }
        if let Some(t) = &b.tail {
            self.collect_expr(t);
        }
    }

    fn collect_expr(&mut self, e: &Expr) {
        match e {
            Expr::Func(def) => self.collect(&def.body),
            Expr::Do(b) => self.collect(b),
            Expr::If(arms, els) => {
                arms.iter().for_each(|(_, b)| self.collect(b));
                if let Some(b) = els {
                    self.collect(b);
                }
            }
            Expr::Match(_, arms) => arms.iter().for_each(|a| self.collect(&a.body)),
            _ => {}
        }
    }

    /// The name of the shape a type stands for, following aliases.
    fn shape_of(&self, t: &Type) -> Option<Rc<str>> {
        let mut cur = t.clone();
        for _ in 0..16 {
            let Type::Named(n, _, _) = &cur else { return None };
            // a name with methods is the answer; so is one whose body is a
            // shape, even with nothing implemented for it, because a field of
            // it may lead somewhere that does have them
            if self.methods.contains_key(n) {
                return Some(n.clone());
            }
            match self.aliases.get(n) {
                // `type Alias = Vec2` — one name for another, so follow it
                Some(next @ Type::Named(..)) => cur = next.clone(),
                Some(_) => return Some(n.clone()),
                None => return None,
            }
        }
        None
    }

    fn bind(&mut self, name: &Name, from: Option<Rc<str>>) {
        let shape = name.ty.as_ref().and_then(|t| self.shape_of(t)).or(from);
        if let (Some(s), Some(scope)) = (shape, self.scopes.last_mut()) {
            scope.insert(name.text.clone(), s);
        }
    }

    fn shape_in_scope(&self, name: &str) -> Option<Rc<str>> {
        self.scopes.iter().rev().find_map(|s| s.get(name)).cloned()
    }

    /// What shape this expression is, when that is written down anywhere.
    fn shape(&self, e: &Expr) -> Option<Rc<str>> {
        match e {
            Expr::Var(n) => self.shape_in_scope(&n.text),
            // `o.inner` — a field whose declared type is itself a shape
            Expr::Index(base, key) => {
                let Expr::Str(field) = &**key else { return None };
                let outer = self.shape(base)?;
                let Type::Record(fields, _) = self.aliases.get(&outer)? else { return None };
                let ft = fields.iter().find(|(n, _)| n.text == *field).map(|(_, t)| t)?;
                self.shape_of(ft)
            }
            Expr::Call(f, _) => match &**f {
                // `make(3, 4)` where `fn make(..) -> Vec2`
                Expr::Var(n) => self.returns.get(&n.text).cloned(),
                // `Vec2::new(3, 4)` and `Vec2::scaled(v, 2)` — including the
                // one a `.` just became, which is how a chain keeps its
                // footing. This pass runs before names are resolved, so the
                // shape is still written as an ordinary name.
                Expr::Index(base, key) => {
                    let shape = match &**base {
                        Expr::Var(n) => Some(&n.text),
                        Expr::Global(n, _) => Some(n),
                        _ => None,
                    };
                    match (shape, &**key) {
                        (Some(shape), Expr::Str(m)) => self
                            .methods
                            .get(shape)
                            .and_then(|ms| ms.iter().find(|(n, _)| n == m))
                            .and_then(|(_, ret)| ret.as_ref())
                            .and_then(|r| self.shape_of(r)),
                        _ => None,
                    }
                }
                _ => None,
            },
            _ => None,
        }
    }

    fn block(&mut self, b: &Block) -> Block {
        self.scopes.push(HashMap::new());
        let stats = b.stats.iter().map(|s| self.stat(s)).collect();
        let tail = b.tail.as_ref().map(|t| Box::new(self.expr(t)));
        self.scopes.pop();
        Block { stats, lines: b.lines.clone(), tail, tail_line: b.tail_line }
    }

    fn stat(&mut self, s: &Stat) -> Stat {
        match s {
            Stat::Let(names, exprs) => {
                let exprs: Vec<Expr> = exprs.iter().map(|e| self.expr(e)).collect();
                let from = (names.len() == 1 && exprs.len() == 1)
                    .then(|| self.shape(&exprs[0]))
                    .flatten();
                for n in names {
                    self.bind(n, from.clone());
                }
                Stat::Let(names.clone(), exprs)
            }
            Stat::Impl(name, params, ms) => Stat::Impl(
                name.clone(),
                params.clone(),
                ms.iter().map(|(m, f)| (m.clone(), self.expr(f))).collect(),
            ),
            Stat::FnDecl(name, f) => Stat::FnDecl(name.clone(), self.expr(f)),
            Stat::Return(es) => Stat::Return(es.iter().map(|e| self.expr(e)).collect()),
            Stat::Assign(ts, es) => Stat::Assign(
                ts.iter().map(|e| self.expr(e)).collect(),
                es.iter().map(|e| self.expr(e)).collect(),
            ),
            Stat::OpAssign(t, op, e) => Stat::OpAssign(self.expr(t), *op, self.expr(e)),
            Stat::Expr(e) => Stat::Expr(self.expr(e)),
            Stat::While(id, c, b) => Stat::While(*id, self.expr(c), self.block(b)),
            Stat::Loop(id, b) => Stat::Loop(*id, self.block(b)),
            Stat::ForRange { id, var, binding, start, end, inclusive, body } => {
                let (start, end) = (self.expr(start), self.expr(end));
                self.scopes.push(HashMap::new());
                self.bind(var, None);
                let body = self.block(body);
                self.scopes.pop();
                Stat::ForRange {
                    id: *id,
                    var: var.clone(),
                    binding: *binding,
                    start,
                    end,
                    inclusive: *inclusive,
                    body,
                }
            }
            Stat::ForIn { id, vars, bindings, iter, body } => {
                let iter = self.expr(iter);
                self.scopes.push(HashMap::new());
                for v in vars {
                    self.bind(v, None);
                }
                let body = self.block(body);
                self.scopes.pop();
                Stat::ForIn {
                    id: *id,
                    vars: vars.clone(),
                    bindings: bindings.clone(),
                    iter,
                    body,
                }
            }
            other => other.clone(),
        }
    }

    fn expr(&mut self, e: &Expr) -> Expr {
        match e {
            // the one this exists for
            Expr::Method(obj, name, args) => {
                let obj = self.expr(obj);
                let args: Vec<Expr> = args.iter().map(|a| self.expr(a)).collect();
                if let Some(shape) = self.shape(&obj) {
                    let has = self
                        .methods
                        .get(&shape)
                        .is_some_and(|ms| ms.iter().any(|(m, _)| m == name));
                    if has {
                        // `Vec2::len(v)`, with the receiver as the first
                        // argument — which is what `v.len()` already meant
                        let mut all = vec![obj];
                        all.extend(args);
                        return Expr::Call(
                            Box::new(Expr::Index(
                                Box::new(Expr::Global(shape, GlobalCache::new())),
                                Box::new(Expr::Str(name.clone())),
                            )),
                            all,
                        );
                    }
                }
                Expr::Method(Box::new(obj), name.clone(), args)
            }
            Expr::Func(def) => {
                self.scopes.push(HashMap::new());
                for p in &def.params {
                    self.bind(p, None);
                }
                let body = self.block(&def.body);
                self.scopes.pop();
                Expr::Func(Rc::new(crate::ast::FuncDef {
                    id: def.id,
                    name: def.name.clone(),
                    params: def.params.clone(),
                    body,
                    ret: def.ret.clone(),
                    line: def.line,
                    n_slots: def.n_slots,
                    param_bindings: def.param_bindings.clone(),
                    upvals: def.upvals.clone(),
                }))
            }
            Expr::Call(f, args) => Expr::Call(
                Box::new(self.expr(f)),
                args.iter().map(|a| self.expr(a)).collect(),
            ),
            Expr::Index(a, b) => {
                Expr::Index(Box::new(self.expr(a)), Box::new(self.expr(b)))
            }
            Expr::Bin(op, a, b) => {
                Expr::Bin(*op, Box::new(self.expr(a)), Box::new(self.expr(b)))
            }
            Expr::Un(op, a) => Expr::Un(*op, Box::new(self.expr(a))),
            Expr::Array(items) => Expr::Array(items.iter().map(|i| self.expr(i)).collect()),
            Expr::Map(entries) => Expr::Map(
                entries.iter().map(|(k, v)| (self.expr(k), self.expr(v))).collect(),
            ),
            Expr::Range(a, b, inc) => {
                Expr::Range(Box::new(self.expr(a)), Box::new(self.expr(b)), *inc)
            }
            Expr::If(arms, els) => Expr::If(
                arms.iter().map(|(c, b)| (self.expr(c), self.block(b))).collect(),
                els.as_ref().map(|b| self.block(b)),
            ),
            Expr::Match(subject, arms) => Expr::Match(
                Box::new(self.expr(subject)),
                arms.iter()
                    .map(|a| crate::ast::Arm {
                        patterns: a.patterns.clone(),
                        guard: a.guard.as_ref().map(|g| self.expr(g)),
                        body: self.block(&a.body),
                    })
                    .collect(),
            ),
            Expr::Do(b) => Expr::Do(self.block(b)),
            other => other.clone(),
        }
    }
}
