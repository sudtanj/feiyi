//! Per-file extraction for the code graph: declarations (reusing outline.rs's
//! keyword heuristics) and the names a file refers to and imports. Plain line
//! matching, no parser.
//! ponytail: a reference is a name followed by `(`, `::`, an uppercase name
//! followed by `<` or `.`, or a name after `new`; strings are skipped, block
//! comments are not. Add tree-sitter when these misses start to matter.

use crate::outline::{declarations, ident_call, lang, starts_any, strip_any, Lang};
use std::collections::BTreeSet;
use std::path::Path;

pub struct Symbol {
    pub kind: &'static str,
    pub name: String,
    pub start: usize,
    pub end: usize,
    /// The enclosing declaration's name (an impl counts as its type).
    pub parent: Option<String>,
}

pub struct Call {
    /// Index into `Parsed::symbols`; None for a call outside any symbol (the file itself).
    pub owner: Option<usize>,
    pub name: String,
}

#[derive(Default)]
pub struct Parsed {
    pub symbols: Vec<Symbol>,
    pub calls: Vec<Call>,
    /// One entry per import: slash-separated path candidates, best first.
    pub imports: Vec<Vec<String>>,
    pub lines: usize,
}

const NOT_CALLS: [&str; 26] = [
    "if", "for", "while", "switch", "catch", "return", "else", "do", "try", "with", "new", "await", "match", "loop", "sizeof", "fn", "def", "function", "impl", "in", "and", "or", "not", "yield", "throw", "typeof",
];

fn ident(s: &str) -> Option<&str> {
    let n = s.find(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$')).unwrap_or(s.len());
    (n > 0).then(|| &s[..n])
}

fn name_of(s: &str) -> Option<String> {
    ident(s).map(String::from)
}

/// The name after the keyword of a declaration line (`kind`, `name`); an impl is named after its type.
fn decl_name(lang: Lang, line: &str) -> Option<(&'static str, String)> {
    let t = line.trim();
    match lang {
        Lang::Rust => {
            let t = strip_any(t, &["pub(crate) ", "pub(super) ", "pub(in ", "pub ", "async ", "unsafe ", "extern \"C\" ", "default "]);
            if let Some(rest) = t.strip_prefix("impl").filter(|r| r.starts_with([' ', '<'])) {
                return impl_type(rest).map(|n| ("impl", n));
            }
            const KINDS: [(&str, &str); 10] = [("fn ", "fn"), ("struct ", "struct"), ("enum ", "enum"), ("trait ", "trait"), ("type ", "type"), ("mod ", "mod"), ("macro_rules! ", "macro"), ("const ", "const"), ("static ", "static"), ("union ", "union")];
            let (kw, kind) = KINDS.iter().find(|(kw, _)| t.starts_with(kw))?;
            let rest = &t[kw.len()..];
            let rest = if *kind == "static" { rest.strip_prefix("mut ").unwrap_or(rest) } else { rest };
            let (kind, rest) = match rest.strip_prefix("fn ") {
                Some(r) if *kind == "const" => ("fn", r),
                _ => (*kind, rest),
            };
            if kind == "mod" && t.ends_with(';') {
                return None;
            }
            name_of(rest.trim_start()).map(|n| (kind, n))
        }
        Lang::Python => {
            let t = strip_any(t, &["async "]);
            [("def ", "fn"), ("class ", "class")].iter().find_map(|(kw, kind)| t.strip_prefix(kw).and_then(|r| name_of(r.trim_start())).map(|n| (*kind, n)))
        }
        Lang::Go => {
            if let Some(rest) = t.strip_prefix("func ") {
                let (rest, kind) = match rest.strip_prefix('(') {
                    Some(r) => (r.split_once(')')?.1, "method"),
                    None => (rest, "fn"),
                };
                return name_of(rest.trim_start()).map(|n| (kind, n));
            }
            t.strip_prefix("type ").and_then(|r| name_of(r.trim_start())).map(|n| ("type", n))
        }
        Lang::Script => {
            let t = strip_any(t, &["export ", "default ", "declare ", "abstract ", "async "]);
            const KINDS: [(&str, &str); 9] = [("function* ", "fn"), ("function ", "fn"), ("class ", "class"), ("interface ", "interface"), ("enum ", "enum"), ("namespace ", "namespace"), ("type ", "type"), ("const ", "const"), ("let ", "let")];
            if let Some((kw, kind)) = KINDS.iter().find(|(kw, _)| t.starts_with(kw)) {
                return name_of(t[kw.len()..].trim_start()).map(|n| (*kind, n));
            }
            let m = strip_any(t, &["static ", "private ", "public ", "protected ", "readonly ", "override ", "get ", "set ", "async ", "*"]);
            ident_call(m).then(|| name_of(m)).flatten().map(|n| ("method", n))
        }
        Lang::C => {
            const KINDS: [(&str, &str); 5] = [("struct ", "struct"), ("enum ", "enum"), ("union ", "union"), ("class ", "class"), ("namespace ", "namespace")];
            if !t.contains('(') {
                if let Some((kw, kind)) = KINDS.iter().find(|(kw, _)| t.starts_with(kw)) {
                    return name_of(t[kw.len()..].trim_start()).map(|n| (*kind, n));
                }
                let rest = t.strip_prefix("typedef ")?;
                let last = rest.trim_end_matches([';', '{', ' ']).rsplit(|c: char| !(c.is_alphanumeric() || c == '_')).next()?;
                return (!last.is_empty() && !["struct", "enum", "union"].contains(&last)).then(|| ("type", last.to_string()));
            }
            let before = t.split('(').next()?.trim_end();
            let name = before.rsplit(|c: char| !(c.is_alphanumeric() || c == '_')).next()?;
            (!name.is_empty() && !t.starts_with("typedef ")).then(|| ("fn", name.to_string()))
        }
        Lang::Shell => {
            let rest = t.strip_prefix("function ").unwrap_or(t);
            name_of(rest.trim_start()).map(|n| ("fn", n))
        }
    }
}

/// `impl<T> Trait for Type<T> {` to `Type`.
fn impl_type(rest: &str) -> Option<String> {
    let mut rest = rest.trim_start();
    if rest.starts_with('<') {
        let mut depth = 0;
        let close = rest.char_indices().find(|(_, c)| {
            depth += match c {
                '<' => 1,
                '>' => -1,
                _ => 0,
            };
            depth == 0
        });
        rest = rest[close?.0 + 1..].trim_start();
    }
    let head = rest.split('{').next()?;
    let head = head.split(" where ").next()?;
    let target = head.rsplit_once(" for ").map(|(_, t)| t).unwrap_or(head);
    let target = target.trim_start_matches(['&', ' ']);
    name_of(target.strip_prefix("dyn ").unwrap_or(target))
}

fn is_comment(t: &str) -> bool {
    t.starts_with('#') || t.starts_with("//") || t.starts_with("/*") || t.starts_with('*')
}

/// Names referred to on one line (see the module note), skipping `skip` (the line's own declaration).
fn refs(line: &str, skip: Option<&str>, out: &mut Vec<String>) {
    let b = line.as_bytes();
    let (mut i, mut after_new) = (0, false);
    while i < b.len() {
        let c = b[i];
        if c == b'"' {
            i += 1;
            while i < b.len() && b[i] != b'"' {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i += 1;
            after_new = false;
        } else if c.is_ascii_digit() {
            while i < b.len() && b[i].is_ascii_alphanumeric() {
                i += 1;
            }
        } else if c.is_ascii_alphabetic() || c == b'_' || c == b'$' {
            let start = i;
            while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$') {
                i += 1;
            }
            let word = &line[start..i];
            let upper = c.is_ascii_uppercase();
            let hit = match b.get(i) {
                Some(b'(') => true,
                Some(b'<') => upper,
                Some(b':') => b.get(i + 1) == Some(&b':'),
                Some(b'.') => upper && b.get(i + 1).is_some_and(|n| n.is_ascii_alphabetic() || *n == b'_'),
                _ => false,
            };
            if (hit || after_new) && !NOT_CALLS.contains(&word) && Some(word) != skip {
                out.push(word.to_string());
            }
            after_new = word == "new";
        } else {
            if !c.is_ascii_whitespace() {
                after_new = false;
            }
            i += 1;
        }
    }
}

/// `a/b/c` to `[a/b/c, a/b, a]`: the tail of an import may be an item, not a module.
fn prefixes(segments: &[&str]) -> Vec<String> {
    (1..=segments.len()).rev().map(|k| segments[..k].join("/")).collect()
}

fn strip_relative(mut p: &str) -> &str {
    while let Some(rest) = p.strip_prefix("./").or_else(|| p.strip_prefix("../")) {
        p = rest;
    }
    p
}

fn quoted(s: &str) -> Option<&str> {
    let start = s.find(['"', '\'', '`'])?;
    let quote = s[start..].chars().next()?;
    let inner = &s[start + 1..];
    inner.find(quote).map(|end| &inner[..end])
}

/// The imports a line makes, as candidate lists. `go_block`: inside `import (`.
fn imports_of(lang: Lang, line: &str, go_block: &mut bool) -> Vec<Vec<String>> {
    let t = line.trim();
    match lang {
        Lang::Rust => {
            let t = strip_any(t, &["pub(crate) ", "pub(super) ", "pub(in ", "pub "]);
            if let Some(m) = t.strip_prefix("mod ").filter(|_| t.ends_with(';')) {
                return name_of(m.trim_start()).map(|n| vec![vec![n]]).unwrap_or_default();
            }
            let Some(path) = t.strip_prefix("use ") else { return vec![] };
            let path = path.split(['{', ';', ' ']).next().unwrap_or("");
            let segments: Vec<&str> = path.split("::").filter(|s| !s.is_empty() && !["crate", "self", "super"].contains(s)).collect();
            if segments.is_empty() || ["std", "core", "alloc"].contains(&segments[0]) {
                return vec![];
            }
            vec![prefixes(&segments)]
        }
        Lang::Python => {
            if let Some(rest) = t.strip_prefix("import ") {
                return rest
                    .split(',')
                    .filter_map(|m| m.split_whitespace().next())
                    .map(|m| prefixes(&m.split('.').filter(|s| !s.is_empty()).collect::<Vec<_>>()))
                    .filter(|c| !c.is_empty())
                    .collect();
            }
            let Some(rest) = t.strip_prefix("from ") else { return vec![] };
            let Some((module, items)) = rest.split_once(" import ") else { return vec![] };
            let module: Vec<&str> = module.split('.').filter(|s| !s.is_empty()).collect();
            let items = items.trim_matches(['(', ')', ' ']);
            items
                .split(',')
                .filter_map(|i| i.split_whitespace().next())
                .take(8)
                .map(|item| {
                    let mut path = module.clone();
                    path.push(item);
                    prefixes(&path)
                })
                .collect()
        }
        Lang::Go => {
            let path = if t.starts_with("import (") {
                *go_block = true;
                None
            } else if let Some(rest) = t.strip_prefix("import ") {
                quoted(rest)
            } else if *go_block && t.starts_with(')') {
                *go_block = false;
                None
            } else if *go_block {
                quoted(t)
            } else {
                None
            };
            let Some(path) = path else { return vec![] };
            let segments: Vec<&str> = path.split('/').collect();
            let keep = if segments.len() >= 2 { segments.len() - 1 } else { 1 };
            vec![(0..keep).map(|from| segments[from..].join("/")).collect()]
        }
        Lang::Script => {
            let module = if starts_any(t, &["import ", "export "]) || (t.starts_with('}') && t.contains(" from ")) {
                t.rsplit_once(" from ").map(|(_, m)| m).or_else(|| t.strip_prefix("import ")).and_then(quoted)
            } else {
                t.find("require(").or_else(|| t.find("import(")).and_then(|at| quoted(&t[at..]))
            };
            let Some(module) = module.filter(|m| m.starts_with('.')) else { return vec![] };
            let module = strip_relative(module);
            let module = ["js", "jsx", "ts", "tsx", "mjs", "cjs", "mts", "cts"].iter().find_map(|e| module.strip_suffix(&format!(".{e}"))).unwrap_or(module);
            vec![vec![module.to_string()]]
        }
        Lang::C => {
            let Some(rest) = t.strip_prefix('#').map(str::trim_start).and_then(|r| r.strip_prefix("include")) else { return vec![] };
            let rest = rest.trim_start();
            let inner = quoted(rest).or_else(|| rest.strip_prefix('<').and_then(|r| r.split('>').next()));
            inner.map(|p| vec![vec![strip_relative(p).to_string()]]).unwrap_or_default()
        }
        Lang::Shell => vec![],
    }
}

/// None when `path` has no known language.
pub fn parse(path: &Path, text: &str) -> Option<Parsed> {
    let lang = lang(path)?;
    let lines: Vec<&str> = text.lines().collect();
    let decls = declarations(lang, &lines);
    let mut out = Parsed { lines: lines.len(), ..Parsed::default() };
    // Per declaration: its symbol index (None for impls and unnamed ones) and name.
    let mut slots: Vec<(Option<usize>, Option<String>)> = Vec::with_capacity(decls.len());
    let mut scopes: Vec<(usize, String)> = Vec::new();
    for d in &decls {
        while scopes.last().is_some_and(|(indent, _)| *indent >= d.indent) {
            scopes.pop();
        }
        let parent = scopes.last().map(|(_, n)| n.clone());
        let Some((kind, name)) = decl_name(lang, lines[d.line]) else {
            slots.push((None, None));
            continue;
        };
        scopes.push((d.indent, name.clone()));
        if kind == "impl" {
            slots.push((None, Some(name)));
            continue;
        }
        let kind = if kind == "fn" && parent.is_some() { "method" } else { kind };
        slots.push((Some(out.symbols.len()), Some(name.clone())));
        out.symbols.push(Symbol { kind, name, start: d.line + 1, end: d.end, parent });
    }

    let mut open: Vec<(usize, Option<usize>)> = Vec::new();
    let mut next = 0;
    let mut go_block = false;
    let mut seen = BTreeSet::new();
    let mut names = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        while open.last().is_some_and(|(end, _)| *end <= i) {
            open.pop();
        }
        let mut skip = None;
        if decls.get(next).is_some_and(|d| d.line == i) {
            open.push((decls[next].end, slots[next].0));
            skip = slots[next].1.as_deref();
            next += 1;
        }
        let imported = out.imports.len();
        out.imports.extend(imports_of(lang, line, &mut go_block));
        if out.imports.len() > imported {
            continue;
        }
        let t = line.trim_start();
        if t.is_empty() || is_comment(t) {
            continue;
        }
        let owner = open.iter().rev().find_map(|(_, s)| *s);
        names.clear();
        refs(line, skip, &mut names);
        for name in names.drain(..) {
            if seen.insert((owner, name.clone())) {
                out.calls.push(Call { owner, name });
            }
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(file: &str, src: &str) -> Parsed {
        parse(Path::new(file), src).unwrap()
    }

    fn names(p: &Parsed) -> Vec<String> {
        p.symbols.iter().map(|s| format!("{} {}{}", s.kind, s.parent.as_ref().map(|x| format!("{x}.")).unwrap_or_default(), s.name)).collect()
    }

    fn called(p: &Parsed, owner: &str) -> Vec<String> {
        p.calls.iter().filter(|c| c.owner.map(|o| p.symbols[o].name.as_str()) == Some(owner)).map(|c| c.name.clone()).collect()
    }

    #[test]
    fn rust_symbols_parents_calls_and_imports() {
        let src = "use crate::backup::Turso;\nuse std::fs;\nmod graph;\n\npub struct A {\n    b: u32,\n}\n\nimpl<T> Run for A<T> {\n    pub fn go(&self) {\n        helper(1);\n        Turso::run();\n        let v = Vec::<u8>::new();\n        println!(\"skipped(x)\");\n    }\n}\n\nfn helper(n: u32) {}\n";
        let p = run("a.rs", src);
        assert_eq!(names(&p), ["struct A", "method A.go", "fn helper"]);
        assert_eq!((p.symbols[1].start, p.symbols[1].end), (10, 16));
        let go = called(&p, "go");
        assert!(go.contains(&"helper".to_string()) && go.contains(&"Turso".to_string()) && go.contains(&"Vec".to_string()), "{go:?}");
        assert!(!go.contains(&"skipped".to_string()));
        assert!(called(&p, "helper").is_empty(), "a declaration is not a call of itself");
        assert_eq!(p.imports, vec![vec!["backup/Turso".to_string(), "backup".to_string()], vec!["graph".to_string()]]);
    }

    #[test]
    fn python_classes_methods_and_imports() {
        let src = "import os.path, json\nfrom pkg.mod import thing as t, other\nfrom . import sib\n\nclass Repo:\n    def save(self):\n        write(self.path)\n        Cache.clear()\n\ndef write(p):\n    pass\n";
        let p = run("r.py", src);
        assert_eq!(names(&p), ["class Repo", "method Repo.save", "fn write"]);
        assert_eq!(called(&p, "save"), ["write", "Cache", "clear"]);
        assert_eq!(p.imports[0], ["os/path", "os"]);
        assert_eq!(p.imports[2], ["pkg/mod/thing", "pkg/mod", "pkg"]);
        assert_eq!(p.imports[4], ["sib"]);
    }

    #[test]
    fn go_funcs_methods_and_import_blocks() {
        let src = "package x\n\nimport (\n\t\"fmt\"\n\tu \"github.com/o/r/pkg/util\"\n)\n\ntype Srv struct{}\n\nfunc (s *Srv) Start() {\n\tutil.Run()\n\tstop()\n}\n\nfunc stop() {}\n";
        let p = run("s.go", src);
        assert_eq!(names(&p), ["type Srv", "method Start", "fn stop"]);
        assert_eq!(p.imports.len(), 2);
        assert_eq!(p.imports[0], ["fmt"]);
        assert_eq!(p.imports[1].last().unwrap(), "pkg/util");
        assert!(called(&p, "Start").contains(&"stop".to_string()));
    }

    #[test]
    fn script_imports_classes_and_new() {
        let src = "import { a } from './lib/a.js';\nimport React from 'react';\nconst b = require('../b');\n\nexport class Store {\n  load(id) {\n    return new Item(id).save();\n  }\n}\n\nexport function boot() {\n  Store.open();\n}\n";
        let p = run("s.ts", src);
        assert_eq!(names(&p), ["class Store", "method Store.load", "fn boot"]);
        assert_eq!(p.imports, vec![vec!["lib/a".to_string()], vec!["b".to_string()]]);
        assert!(called(&p, "load").contains(&"Item".to_string()));
        assert!(called(&p, "boot").contains(&"Store".to_string()));
    }

    #[test]
    fn c_functions_types_and_includes() {
        let src = "#include \"net/sock.h\"\n#include <stdio.h>\n\nstruct dev {\n    int id;\n};\n\ntypedef struct dev dev_t2;\n\nstatic int probe(struct dev *d)\n{\n    return init_dev(d);\n}\n";
        let p = run("d.c", src);
        assert_eq!(names(&p), ["struct dev", "type dev_t2", "fn probe"]);
        assert_eq!(p.imports, vec![vec!["net/sock.h".to_string()], vec!["stdio.h".to_string()]]);
        assert_eq!(called(&p, "probe"), ["init_dev"]);
    }

    #[test]
    fn shell_functions() {
        let p = run("x.sh", "build() {\n  compile\n  lint_all x\n}\n\nfunction lint_all {\n  echo hi\n}\n");
        assert_eq!(names(&p), ["fn build", "fn lint_all"]);
    }

    #[test]
    fn calls_outside_symbols_belong_to_the_file_and_comments_are_skipped() {
        let p = run("m.py", "# call_in_comment(1)\nsetup()\nx = \"not_a_call(1)\"\n");
        assert_eq!(p.calls.len(), 1);
        assert!(p.calls[0].owner.is_none() && p.calls[0].name == "setup");
        assert!(parse(Path::new("README.md"), "x").is_none());
    }
}
