//! What the file says about its own types.
//!
//! Nothing here infers anything. It reads the annotations that were written
//! and the names given to shapes, so that the editor can answer the two
//! questions that otherwise send you to the source: what goes in this field,
//! and what does this argument want.

use rua_syntax::ast::{Block, Expr, FuncDef, Name, Stat, Type};
use std::collections::HashMap;
use std::rc::Rc;

/// A function's parameters and what it hands back, as written.
#[derive(Clone)]
pub struct Signature {
    pub name: Rc<str>,
    pub params: Vec<Name>,
    pub ret: Option<Type>,
}

impl Signature {
    /// `add(left: number, right: number) -> number`, the way it was written.
    pub fn label(&self) -> String {
        let params: Vec<String> = self
            .params
            .iter()
            .map(|p| match &p.ty {
                Some(t) => format!("{p}: {t}"),
                None => p.to_string(),
            })
            .collect();
        match &self.ret {
            Some(r) => format!("{}({}) -> {r}", self.name, params.join(", ")),
            None => format!("{}({})", self.name, params.join(", ")),
        }
    }
}

#[derive(Default)]
pub struct Types {
    /// `type Point = #{ .. }`
    aliases: HashMap<Rc<str>, Type>,
    /// The type written beside a binding, by the name's own span.
    at_decl: HashMap<(u32, u32), Type>,
    /// Functions written in this file.
    functions: Vec<Signature>,
}

impl Types {
    pub fn read(block: &Block) -> Types {
        let mut t = Types::default();
        t.block(block);
        t
    }

    /// What a shape is called, once the names are followed. A name for a name
    /// for a shape is still that shape; a cycle answers nothing rather than
    /// spinning.
    pub fn resolve<'a>(&'a self, ty: &'a Type) -> Option<&'a Type> {
        let mut seen = 0;
        let mut cur = ty;
        loop {
            match cur {
                Type::Named(n, _, _) => match self.aliases.get(n) {
                    Some(next) => {
                        seen += 1;
                        if seen > 16 {
                            return None;
                        }
                        cur = next;
                    }
                    None => return Some(cur),
                },
                other => return Some(other),
            }
        }
    }

    /// The fields of a shape, if that is what it is.
    pub fn fields<'a>(&'a self, ty: &'a Type) -> Option<&'a [(Name, Type)]> {
        match self.resolve(ty)? {
            Type::Record(fields, _) => Some(fields),
            _ => None,
        }
    }

    /// The type written where this name was declared.
    pub fn at(&self, decl: rua_syntax::ast::Span) -> Option<&Type> {
        self.at_decl.get(&(decl.lo, decl.hi))
    }

    pub fn alias_names(&self) -> Vec<Rc<str>> {
        self.aliases.keys().cloned().collect()
    }

    pub fn alias(&self, name: &str) -> Option<&Type> {
        self.aliases.get(name)
    }

    pub fn function(&self, name: &str) -> Option<&Signature> {
        self.functions.iter().find(|f| &*f.name == name)
    }

    pub fn functions(&self) -> &[Signature] {
        &self.functions
    }

    // ---- reading the tree ------------------------------------------------

    fn note(&mut self, n: &Name) {
        if let Some(t) = &n.ty {
            self.at_decl.insert((n.span.lo, n.span.hi), t.clone());
        }
    }

    fn block(&mut self, b: &Block) {
        for s in &b.stats {
            self.stat(s);
        }
        if let Some(t) = &b.tail {
            self.expr(t);
        }
    }

    fn stat(&mut self, s: &Stat) {
        match s {
            Stat::TypeAlias(name, t) => {
                self.aliases.insert(name.text.clone(), t.clone());
            }
            Stat::Let(names, es) => {
                for n in names {
                    self.note(n);
                }
                es.iter().for_each(|e| self.expr(e));
            }
            Stat::LetSlots(_, es) | Stat::Return(es) => es.iter().for_each(|e| self.expr(e)),
            Stat::FnDecl(name, f) => {
                if let Expr::Func(def) = f {
                    self.function_def(&name.text, def);
                }
                self.expr(f);
            }
            Stat::FnSlot(_, f) | Stat::Expr(f) => self.expr(f),
            Stat::Assign(ts, es) => {
                ts.iter().chain(es).for_each(|e| self.expr(e));
            }
            Stat::OpAssign(t, _, e) => {
                self.expr(t);
                self.expr(e);
            }
            Stat::While(_, c, b) => {
                self.expr(c);
                self.block(b);
            }
            Stat::Loop(_, b) => self.block(b),
            Stat::ForRange { var, start, end, body, .. } => {
                self.note(var);
                self.expr(start);
                self.expr(end);
                self.block(body);
            }
            Stat::ForIn { vars, iter, body, .. } => {
                vars.iter().for_each(|v| self.note(v));
                self.expr(iter);
                self.block(body);
            }
            Stat::Break | Stat::Continue => {}
        }
    }

    fn function_def(&mut self, name: &str, def: &Rc<FuncDef>) {
        self.functions.push(Signature {
            name: name.into(),
            params: def.params.clone(),
            ret: def.ret.clone(),
        });
    }

    fn expr(&mut self, e: &Expr) {
        match e {
            Expr::Func(def) => {
                for p in &def.params {
                    self.note(p);
                }
                if !def.name.is_empty() {
                    let n = def.name.clone();
                    self.function_def(&n, def);
                }
                self.block(&def.body);
            }
            Expr::Call(f, args) => {
                self.expr(f);
                args.iter().for_each(|a| self.expr(a));
            }
            Expr::Method(o, _, args) => {
                self.expr(o);
                args.iter().for_each(|a| self.expr(a));
            }
            Expr::Index(a, b) | Expr::Bin(_, a, b) => {
                self.expr(a);
                self.expr(b);
            }
            Expr::Un(_, a) => self.expr(a),
            Expr::Array(es) => es.iter().for_each(|e| self.expr(e)),
            Expr::Map(entries) => entries.iter().for_each(|(k, v)| {
                self.expr(k);
                self.expr(v);
            }),
            Expr::Range(a, b, _) => {
                self.expr(a);
                self.expr(b);
            }
            Expr::If(arms, els) => {
                for (c, b) in arms {
                    self.expr(c);
                    self.block(b);
                }
                if let Some(b) = els {
                    self.block(b);
                }
            }
            Expr::Match(subject, arms) => {
                self.expr(subject);
                for a in arms {
                    self.block(&a.body);
                }
            }
            Expr::Do(b) => self.block(b),
            _ => {}
        }
    }
}
