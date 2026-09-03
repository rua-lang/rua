//! Check the types that were written.
//!
//! Gradual, and quiet by default: a name with no type is `any`, and `any`
//! fits everywhere in both directions. Nothing here infers a type for an
//! unannotated binding and then holds somebody to it — the only complaints
//! are about the promises a program made itself, and only where both sides
//! are known. A checker that guesses is a checker people turn off.
//!
//! Structural, because rua's values are: a table with the right fields is the
//! shape, whatever it was called on the way in.

use crate::ast::{BinOp, Block, Expr, FuncDef, Name, Span, Stat, Type, UnOp};
use crate::SyntaxError;
use std::collections::HashMap;
use std::rc::Rc;

/// Everything the checker found wrong, in source order.
pub fn check(block: &Block) -> Vec<SyntaxError> {
    let mut cx = Checker::default();
    cx.collect_types(block);
    cx.scopes.push(HashMap::new());
    cx.collect_impls(block);
    cx.collect_functions(block);
    cx.block(block);
    cx.found.sort_by_key(|e| (e.line, e.span.lo, e.span.hi));
    cx.found
}

fn named(word: &str, span: Span) -> Type {
    Type::Named(word.into(), Vec::new(), span)
}

/// `any` fits everywhere, in both directions. It is what a program says when
/// it has not said.
fn any(span: Span) -> Type {
    named("any", span)
}

fn is_any(t: &Type) -> bool {
    matches!(t, Type::Named(n, _, _) if &**n == "any")
}

#[derive(Default)]
struct Checker {
    aliases: HashMap<Rc<str>, (Vec<Name>, Type)>,
    /// The names a `type` takes, which stand for anything inside its body.
    open: Vec<Rc<str>>,
    scopes: Vec<HashMap<Rc<str>, Type>>,
    /// What each function this file declares takes and gives back.
    signatures: HashMap<Rc<str>, (Vec<Option<Type>>, Option<Type>)>,
    /// The methods `impl` gave a shape: not fields of it, but reached the
    /// same way, so they are checked the same way.
    impls: HashMap<Rc<str>, Vec<(Rc<str>, Type)>>,
    /// What the function being checked promised to return.
    returning: Vec<Option<Type>>,
    /// The line of the statement being checked. Most expressions carry no
    /// span of their own, and a complaint has to land somewhere a reader can
    /// see; the line it was written on is that place.
    line: u32,
    found: Vec<SyntaxError>,
}

impl Checker {
    fn say(&mut self, message: impl Into<String>, span: Span) {
        // one complaint per place: a wrong type usually reads wrong twice
        if !span.is_empty()
            && self.found.iter().any(|e| e.span.lo == span.lo && e.span.hi == span.hi)
        {
            return;
        }
        let line = self.line;
        if span.is_empty() && self.found.iter().any(|e| e.line == line) {
            return;
        }
        self.found.push(SyntaxError::new(message, line, span));
    }

    // ---- gathering what the file declares --------------------------------

    fn collect_types(&mut self, b: &Block) {
        for s in &b.stats {
            if let Stat::TypeAlias(name, params, t) = s {
                self.aliases.insert(name.text.clone(), (params.clone(), t.clone()));
            }
        }
        // and check the bodies once every name is known
        let names: Vec<Rc<str>> = self.aliases.keys().cloned().collect();
        for n in names {
            let (params, body) = self.aliases[&n].clone();
            self.open = params.iter().map(|p| p.text.clone()).collect();
            self.known(&body);
            self.open.clear();
        }
    }

    fn collect_impls(&mut self, b: &Block) {
        for s in &b.stats {
            let Stat::Impl(name, methods) = s else { continue };
            let found: Vec<(Rc<str>, Type)> = methods
                .iter()
                .filter_map(|(m, f)| match f {
                    Expr::Func(def) => Some((m.text.clone(), self.signature_of(def))),
                    _ => None,
                })
                .collect();
            self.impls.entry(name.text.clone()).or_default().extend(found);
        }
    }

    /// A method reached on a shape: `impl` first, then a field holding a
    /// function, since both are written `v.len()`.
    fn method_type(&self, receiver: &Type, name: &str) -> Option<Type> {
        if let Type::Named(n, _, _) = receiver {
            if let Some(found) =
                self.impls.get(n).and_then(|ms| ms.iter().find(|(m, _)| &**m == name))
            {
                return Some(found.1.clone());
            }
        }
        let Type::Record(fields, _) = self.expand(receiver, 0) else { return None };
        fields.iter().find(|(n, _)| &*n.text == name).map(|(_, t)| self.expand(t, 0))
    }

    fn collect_functions(&mut self, b: &Block) {
        for s in &b.stats {
            let (name, f) = match s {
                Stat::FnDecl(name, f) => (name.text.clone(), f),
                _ => continue,
            };
            if let Expr::Func(def) = f {
                let params = def.params.iter().map(|p| p.ty.clone()).collect();
                self.signatures.insert(name, (params, def.ret.clone()));
            }
        }
    }

    /// Is every name in this type one the file declares, or one of the words
    /// that need no declaring?
    fn known(&mut self, t: &Type) {
        match t {
            Type::Named(n, args, span) => {
                let builtin = matches!(
                    &**n,
                    "number" | "string" | "boolean" | "nil" | "table" | "function" | "any"
                );
                if !builtin && !self.aliases.contains_key(n) && !self.open.contains(n) {
                    self.say(format!("`{n}` is not a type this program declares"), *span);
                }
                args.iter().for_each(|a| self.known(a));
            }
            Type::Array(inner, _) => self.known(inner),
            Type::Record(fields, _) => fields.iter().for_each(|(_, ft)| self.known(ft)),
            Type::Fn(args, ret, _) => {
                args.iter().for_each(|(_, a)| self.known(a));
                if let Some(r) = ret {
                    self.known(r);
                }
            }
        }
    }

    // ---- following names -------------------------------------------------

    /// A name followed to the shape it stands for, its arguments filled in.
    fn expand(&self, t: &Type, depth: usize) -> Type {
        if depth > 16 {
            return any(t.span());
        }
        let Type::Named(n, args, _) = t else { return t.clone() };
        let Some((params, body)) = self.aliases.get(n) else { return t.clone() };
        if params.is_empty() {
            return self.expand(body, depth + 1);
        }
        if params.len() != args.len() {
            return any(t.span());
        }
        let bound: HashMap<&str, &Type> =
            params.iter().map(|p| &*p.text).zip(args.iter()).collect();
        let filled = substitute(body, &bound);
        self.expand(&filled, depth + 1)
    }

    // ---- does one fit the other? -----------------------------------------

    /// May a value of `from` be used where `to` is wanted? Silent whenever
    /// either side is unknown, which is what makes this gradual.
    fn fits(&self, from: &Type, to: &Type) -> bool {
        self.fits_at(from, to, 0)
    }

    fn fits_at(&self, from: &Type, to: &Type, depth: usize) -> bool {
        if is_any(from) || is_any(to) {
            return true;
        }
        // The same name on both sides needs no following, and following it
        // would not stop: a shape with a method on it names itself, which is
        // what `fn(self: Vec2) -> Vec2` is.
        if let (Type::Named(a, aa, _), Type::Named(b, bb, _)) = (from, to) {
            if a == b && aa.len() == bb.len() {
                return aa.iter().zip(bb).all(|(x, y)| self.fits_at(x, y, depth + 1));
            }
        }
        if depth > 24 {
            return true;
        }
        let (f, t) = (self.expand(from, 0), self.expand(to, 0));
        if is_any(&f) || is_any(&t) {
            return true;
        }
        match (&f, &t) {
            // a name that is still a name is one nobody declared: say nothing
            (Type::Named(a, _, _), Type::Named(b, _, _)) => {
                a == b
                    || (&**b == "table" && false)
                    || !self.aliases.contains_key(a) && !self.aliases.contains_key(b) && {
                        matches!(
                            &**a,
                            "number" | "string" | "boolean" | "nil" | "table" | "function"
                        ) && a == b
                    }
            }
            // a record and an array are both tables
            (Type::Record(..) | Type::Array(..), Type::Named(b, _, _)) => &**b == "table",
            (Type::Fn(..), Type::Named(b, _, _)) => &**b == "function",
            (Type::Named(a, _, _), _) => &**a == "table" || &**a == "function",
            (Type::Array(a, _), Type::Array(b, _)) => self.fits_at(a, b, depth + 1),
            // every field the shape asks for, and of the right type. More
            // than that is fine: a bigger table is still that shape.
            (Type::Record(have, _), Type::Record(want, _)) => want.iter().all(|(n, wt)| {
                have.iter()
                    .find(|(hn, _)| hn.text == n.text)
                    .is_some_and(|(_, ht)| self.fits_at(ht, wt, depth + 1))
            }),
            (Type::Fn(fa, fr, _), Type::Fn(ta, tr, _)) => {
                fa.len() == ta.len()
                    && fa.iter().zip(ta).all(|((_, a), (_, b))| self.fits_at(b, a, depth + 1))
                    && match (fr, tr) {
                        (Some(a), Some(b)) => self.fits_at(a, b, depth + 1),
                        _ => true,
                    }
            }
            _ => false,
        }
    }

    // ---- scopes ----------------------------------------------------------

    fn bind(&mut self, name: &Name, t: Type) {
        if let Some(s) = self.scopes.last_mut() {
            s.insert(name.text.clone(), t);
        }
    }

    fn lookup(&self, name: &str) -> Option<Type> {
        self.scopes.iter().rev().find_map(|s| s.get(name)).cloned()
    }

    // ---- walking ---------------------------------------------------------

    fn block(&mut self, b: &Block) {
        self.scopes.push(HashMap::new());
        let outer = self.line;
        for (i, s) in b.stats.iter().enumerate() {
            self.line = b.lines.get(i).copied().unwrap_or(outer);
            self.stat(s);
        }
        if let Some(t) = &b.tail {
            self.line = if b.tail_line > 0 { b.tail_line } else { outer };
            self.infer(t);
        }
        self.line = outer;
        self.scopes.pop();
    }

    fn stat(&mut self, s: &Stat) {
        match s {
            Stat::TypeAlias(..) => {}
            Stat::Impl(_, methods) => {
                for (_, f) in methods {
                    self.infer(f);
                }
            }
            Stat::Let(names, exprs) => {
                // one name to one value is the case worth checking; the rest
                // spread a call's results and nothing here knows how many
                let one = names.len() == 1 && exprs.len() == 1;
                for (i, e) in exprs.iter().enumerate() {
                    let got = self.infer(e);
                    if one {
                        if let Some(want) = &names[0].ty {
                            self.known(want);
                            if !self.fits(&got, want) {
                                let e_span = span_of(e);
                                self.say(
                                    format!("expected {want}, found {got}"),
                                    e_span,
                                );
                            }
                        }
                    }
                    let _ = i;
                }
                for n in names {
                    let t = match (&n.ty, one) {
                        (Some(t), _) => t.clone(),
                        (None, true) => self.infer(&exprs[0]),
                        _ => any(n.span),
                    };
                    self.bind(n, t);
                }
            }
            Stat::FnDecl(name, f) => {
                if let Expr::Func(def) = f {
                    self.bind(name, self.signature_of(def));
                }
                self.infer(f);
            }
            Stat::Return(es) => {
                let want = self.returning.last().cloned().flatten();
                for e in es {
                    let got = self.infer(e);
                    if let Some(w) = &want {
                        if !self.fits(&got, w) {
                            self.say(format!("expected {w}, found {got}"), span_of(e));
                        }
                    }
                }
                if es.is_empty() {
                    if let Some(w) = &want {
                        if !self.fits(&named("nil", Span::default()), w) {
                            self.say(format!("expected {w}, returned nothing"), Span::default());
                        }
                    }
                }
            }
            Stat::Assign(targets, exprs) => {
                for e in targets.iter().chain(exprs) {
                    self.infer(e);
                }
            }
            Stat::OpAssign(t, _, e) => {
                self.infer(t);
                self.infer(e);
            }
            Stat::Expr(e) => {
                self.infer(e);
            }
            Stat::While(_, c, b) => {
                self.infer(c);
                self.block(b);
            }
            Stat::Loop(_, b) => self.block(b),
            Stat::ForRange { var, start, end, body, .. } => {
                self.infer(start);
                self.infer(end);
                self.scopes.push(HashMap::new());
                self.bind(var, named("number", var.span));
                self.block(body);
                self.scopes.pop();
            }
            Stat::ForIn { vars, iter, body, .. } => {
                self.infer(iter);
                self.scopes.push(HashMap::new());
                for v in vars {
                    let t = v.ty.clone().unwrap_or_else(|| any(v.span));
                    self.bind(v, t);
                }
                self.block(body);
                self.scopes.pop();
            }
            Stat::LetSlots(..) | Stat::FnSlot(..) => {}
            Stat::Break | Stat::Continue => {}
        }
    }

    fn signature_of(&self, def: &FuncDef) -> Type {
        Type::Fn(
            def.params
                .iter()
                .map(|p| (Some(p.clone()), p.ty.clone().unwrap_or_else(|| any(p.span))))
                .collect(),
            def.ret.clone().map(Box::new),
            Span::default(),
        )
    }

    /// The type of an expression, as far as anything written says.
    fn infer(&mut self, e: &Expr) -> Type {
        match e {
            Expr::Num(_) => named("number", span_of(e)),
            Expr::Str(_) => named("string", span_of(e)),
            Expr::Bool(_) => named("boolean", span_of(e)),
            Expr::Nil => named("nil", span_of(e)),
            Expr::Var(n) => self.lookup(&n.text).unwrap_or_else(|| any(n.span)),
            Expr::Array(items) => {
                let mut element: Option<Type> = None;
                for it in items {
                    let t = self.infer(it);
                    element = Some(match element {
                        None => t,
                        Some(prev) if self.fits(&t, &prev) => prev,
                        // a mixed array is an array of anything
                        Some(_) => any(span_of(e)),
                    });
                }
                Type::Array(Box::new(element.unwrap_or_else(|| any(span_of(e)))), span_of(e))
            }
            Expr::Map(entries) => {
                let mut fields = Vec::new();
                for (k, v) in entries {
                    let t = self.infer(v);
                    if let Expr::Str(name) = k {
                        fields.push((Name::new(name.clone(), span_of(e)), t));
                    }
                }
                Type::Record(fields, span_of(e))
            }
            Expr::Func(def) => {
                self.check_function(def);
                self.signature_of(def)
            }
            Expr::Call(f, args) => self.call(f, args, span_of(e)),
            // `v.len()` — the receiver is the first argument, so a method's
            // type names it, and one type describes both `v.len()` and
            // `vec2::len(v)` because they are the same call written twice.
            Expr::Method(obj, name, args) => {
                let receiver = self.infer(obj);
                let given: Vec<(Type, Span)> =
                    args.iter().map(|a| (self.infer(a), span_of(a))).collect();
                // the runtime answers `len` on any table, so a name neither
                // `impl` nor the shape mentions is not an error — a shape
                // says what it has, not what it lacks
                let Some(found) = self.method_type(&receiver, name) else {
                    return any(span_of(e));
                };
                let Type::Fn(params, ret, _) = found else {
                    return any(span_of(e));
                };
                // the receiver takes the first parameter
                let wanted = params.len().saturating_sub(1);
                if wanted != given.len() {
                    self.say(
                        format!(
                            "`{name}` takes {wanted} argument{}, given {}",
                            if wanted == 1 { "" } else { "s" },
                            given.len()
                        ),
                        span_of(e),
                    );
                    return ret.map(|r| *r).unwrap_or_else(|| any(span_of(e)));
                }
                for ((pname, want), (got, span)) in params.iter().skip(1).zip(&given) {
                    if !self.fits(got, want) {
                        let which = match pname {
                            Some(n) => format!("`{n}` expects {want}"),
                            None => format!("expected {want}"),
                        };
                        self.say(format!("{which}, found {got}"), *span);
                    }
                }
                ret.map(|r| *r).unwrap_or_else(|| any(span_of(e)))
            }
            Expr::Index(obj, key) => {
                let base = self.infer(obj);
                let k = self.infer(key);
                match (self.expand(&base, 0), key.as_ref()) {
                    (Type::Record(fields, _), Expr::Str(name)) => fields
                        .iter()
                        .find(|(n, _)| n.text == *name)
                        .map(|(_, t)| t.clone())
                        .unwrap_or_else(|| any(span_of(e))),
                    (Type::Array(inner, _), _) if self.fits(&k, &named("number", Span::default())) => {
                        *inner
                    }
                    _ => any(span_of(e)),
                }
            }
            Expr::Bin(op, a, b) => self.binary(*op, a, b, span_of(e)),
            Expr::Un(op, a) => {
                let t = self.infer(a);
                match op {
                    UnOp::Neg => {
                        self.want(&t, "number", span_of(a));
                        named("number", span_of(e))
                    }
                    UnOp::Not => named("boolean", span_of(e)),
                }
            }
            Expr::Range(a, b, _) => {
                let (ta, tb) = (self.infer(a), self.infer(b));
                self.want(&ta, "number", span_of(a));
                self.want(&tb, "number", span_of(b));
                any(span_of(e))
            }
            Expr::If(arms, els) => {
                for (c, b) in arms {
                    self.infer(c);
                    self.block(b);
                }
                if let Some(b) = els {
                    self.block(b);
                }
                any(span_of(e))
            }
            Expr::Match(subject, arms) => {
                self.infer(subject);
                for a in arms {
                    self.block(&a.body);
                }
                any(span_of(e))
            }
            Expr::Do(b) => {
                self.block(b);
                any(span_of(e))
            }
            Expr::Local(..) | Expr::Upval(..) | Expr::Global(..) => any(span_of(e)),
        }
    }

    fn check_function(&mut self, def: &FuncDef) {
        self.scopes.push(HashMap::new());
        for p in &def.params {
            if let Some(t) = &p.ty {
                self.known(t);
            }
            let t = p.ty.clone().unwrap_or_else(|| any(p.span));
            self.bind(p, t);
        }
        if let Some(r) = &def.ret {
            self.known(r);
        }
        self.returning.push(def.ret.clone());
        // the body's value is a return too
        let outer = self.line;
        for (i, s) in def.body.stats.iter().enumerate() {
            self.line = def.body.lines.get(i).copied().unwrap_or(outer);
            self.stat(s);
        }
        if def.body.tail_line > 0 {
            self.line = def.body.tail_line;
        }
        if let Some(tail) = &def.body.tail {
            let got = self.infer(tail);
            if let Some(w) = &def.ret {
                if !self.fits(&got, w) {
                    self.say(format!("expected {w}, found {got}"), span_of(tail));
                }
            }
        }
        self.line = outer;
        self.returning.pop();
        self.scopes.pop();
    }

    fn call(&mut self, f: &Expr, args: &[Expr], at: Span) -> Type {
        let given: Vec<(Type, Span)> =
            args.iter().map(|a| (self.infer(a), span_of(a))).collect();
        // a function written in this file, called by name
        let signature = match f {
            Expr::Var(n) => self
                .lookup(&n.text)
                .map(|t| self.expand(&t, 0))
                .or_else(|| {
                    self.signatures.get(&n.text).map(|(ps, r)| {
                        Type::Fn(
                            ps.iter()
                                .map(|p| (None, p.clone().unwrap_or_else(|| any(at))))
                                .collect(),
                            r.clone().map(Box::new),
                            at,
                        )
                    })
                }),
            // `Vec2::new(3, 4)` — an `impl` function reached through its
            // shape's name, which is what a constructor is here
            Expr::Index(base, key) => {
                let shape = match &**base {
                    Expr::Var(n) => Some(n.text.clone()),
                    Expr::Global(n, _) => Some(n.clone()),
                    _ => None,
                };
                match (shape, &**key) {
                    (Some(shape), Expr::Str(m)) => self
                        .impls
                        .get(&shape)
                        .and_then(|ms| ms.iter().find(|(n, _)| n == m))
                        .map(|(_, t)| t.clone()),
                    _ => None,
                }
            }
            other => Some(self.expand(&self.infer_ref(other), 0)),
        };
        let Some(Type::Fn(params, ret, _)) = signature else {
            return any(at);
        };
        if params.len() != given.len() {
            let name = match f {
                Expr::Var(n) => format!("`{}`", n.text),
                Expr::Index(_, k) => match &**k {
                    Expr::Str(m) => format!("`{m}`"),
                    _ => "this".to_string(),
                },
                _ => "this".to_string(),
            };
            self.say(
                format!(
                    "{name} takes {} argument{}, given {}",
                    params.len(),
                    if params.len() == 1 { "" } else { "s" },
                    given.len()
                ),
                at,
            );
            return ret.map(|r| *r).unwrap_or_else(|| any(at));
        }
        for ((name, want), (got, span)) in params.iter().zip(&given) {
            if !self.fits(got, want) {
                let which = match name {
                    Some(n) => format!("`{n}` expects {want}"),
                    None => format!("expected {want}"),
                };
                self.say(format!("{which}, found {got}"), *span);
            }
        }
        ret.map(|r| *r).unwrap_or_else(|| any(at))
    }

    /// Infer without recording complaints twice — for a callee already walked.
    fn infer_ref(&self, e: &Expr) -> Type {
        match e {
            Expr::Var(n) => self.lookup(&n.text).unwrap_or_else(|| any(n.span)),
            _ => any(span_of(e)),
        }
    }

    fn binary(&mut self, op: BinOp, a: &Expr, b: &Expr, at: Span) -> Type {
        let (ta, tb) = (self.infer(a), self.infer(b));
        match op {
            // `+` adds numbers and joins strings, so it only complains when
            // neither reading works
            BinOp::Add => {
                let numbers = self.fits(&ta, &named("number", at))
                    && self.fits(&tb, &named("number", at));
                let strings = self.fits(&ta, &named("string", at))
                    || self.fits(&tb, &named("string", at));
                if !numbers && !strings {
                    self.say(format!("`+` needs numbers or a string, found {ta} and {tb}"), at);
                }
                if strings && !numbers {
                    named("string", at)
                } else {
                    named("number", at)
                }
            }
            BinOp::Sub | BinOp::Mul | BinOp::Div | BinOp::Rem => {
                self.want(&ta, "number", span_of(a));
                self.want(&tb, "number", span_of(b));
                named("number", at)
            }
            BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => named("boolean", at),
            BinOp::Eq | BinOp::Ne | BinOp::And | BinOp::Or => named("boolean", at),
        }
    }

    fn want(&mut self, got: &Type, word: &str, at: Span) {
        let wanted = named(word, at);
        if !self.fits(got, &wanted) {
            self.say(format!("expected {word}, found {got}"), at);
        }
    }
}

/// Put the arguments in place of the parameters, all the way down.
fn substitute(t: &Type, bound: &HashMap<&str, &Type>) -> Type {
    match t {
        Type::Named(n, args, s) => match bound.get(&**n) {
            Some(x) if args.is_empty() => (*x).clone(),
            _ => Type::Named(
                n.clone(),
                args.iter().map(|a| substitute(a, bound)).collect(),
                *s,
            ),
        },
        Type::Array(inner, s) => Type::Array(Box::new(substitute(inner, bound)), *s),
        Type::Record(fields, s) => Type::Record(
            fields.iter().map(|(n, ft)| (n.clone(), substitute(ft, bound))).collect(),
            *s,
        ),
        Type::Fn(args, ret, s) => Type::Fn(
            args.iter().map(|(n, a)| (n.clone(), substitute(a, bound))).collect(),
            ret.as_ref().map(|r| Box::new(substitute(r, bound))),
            *s,
        ),
    }
}

/// Where an expression was written. Only the ones that carry a span of their
/// own can say; the rest answer with nothing, and a complaint about them
/// lands on the statement instead.
fn span_of(e: &Expr) -> Span {
    match e {
        Expr::Var(n) => n.span,
        Expr::Index(o, _) => span_of(o),
        Expr::Call(f, _) => span_of(f),
        Expr::Method(o, _, _) => span_of(o),
        Expr::Bin(_, a, _) => span_of(a),
        Expr::Un(_, a) => span_of(a),
        Expr::Map(entries) => entries.first().map(|(k, _)| span_of(k)).unwrap_or_default(),
        Expr::Array(items) => items.first().map(span_of).unwrap_or_default(),
        _ => Span::default(),
    }
}
