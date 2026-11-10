//! `outline`: a source file's declarations with their line ranges, so an
//! agent reads the part it needs instead of the whole file (see the Read
//! guard in claude_stream). Plain line matching, no tokens spent.
//! ponytail: keyword heuristics per language, not a parser; a call graph
//! (callers, impact) needs tree-sitter, add it when outline proves useful.

use std::path::Path;

/// Files longer than this are only read whole on purpose (see `read_guard`).
pub const READ_WHOLE_MAX_LINES: usize = 300;
const MAX_SYMBOLS: usize = 400;

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Lang {
    Rust,
    Python,
    Go,
    Script,
    C,
    Shell,
}

pub(crate) fn lang(path: &Path) -> Option<Lang> {
    Some(match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "rs" => Lang::Rust,
        "py" | "pyi" => Lang::Python,
        "go" => Lang::Go,
        "ts" | "tsx" | "js" | "jsx" | "mjs" | "cjs" | "mts" | "cts" => Lang::Script,
        "c" | "h" | "cc" | "cpp" | "cxx" | "hpp" | "hh" => Lang::C,
        "sh" | "bash" => Lang::Shell,
        _ => return None,
    })
}

pub(crate) fn strip_any<'a>(mut s: &'a str, mods: &[&str]) -> &'a str {
    while let Some(rest) = mods.iter().find_map(|m| s.strip_prefix(m)) {
        s = rest.trim_start();
    }
    s
}

pub(crate) fn starts_any(s: &str, kws: &[&str]) -> bool {
    kws.iter().any(|k| s.starts_with(k))
}

pub(crate) fn ident_call(s: &str) -> bool {
    let name: String = s.chars().take_while(|c| c.is_alphanumeric() || *c == '_' || *c == '$').collect();
    const NOT: [&str; 12] = ["if", "for", "while", "switch", "catch", "return", "else", "do", "try", "with", "new", "await"];
    !name.is_empty() && !NOT.contains(&name.as_str()) && s[name.len()..].trim_start().starts_with(['(', '<'])
}

/// Is `line` (indented by `indent`) a declaration worth listing?
pub(crate) fn is_decl(lang: Lang, line: &str, indent: usize) -> bool {
    let t = line.trim_start();
    match lang {
        Lang::Rust => {
            let t = strip_any(t, &["pub(crate) ", "pub(super) ", "pub(in ", "pub ", "async ", "unsafe ", "extern \"C\" ", "default "]);
            starts_any(t, &["fn ", "struct ", "enum ", "trait ", "impl ", "impl<", "mod ", "type ", "macro_rules! ", "const ", "static ", "union "])
        }
        Lang::Python => starts_any(strip_any(t, &["async "]), &["def ", "class "]),
        Lang::Go => indent == 0 && starts_any(t, &["func ", "type "]),
        Lang::Script => {
            let t = strip_any(t, &["export ", "default ", "declare ", "abstract ", "async "]);
            if starts_any(t, &["function ", "function* ", "class ", "interface ", "enum ", "namespace "]) || (indent == 0 && starts_any(t, &["type ", "const ", "let "])) {
                return true;
            }
            // A method in a class body: `name(…) {`.
            let m = strip_any(t, &["static ", "private ", "public ", "protected ", "readonly ", "override ", "get ", "set ", "async ", "*"]);
            indent > 0 && line.trim_end().ends_with('{') && !t.contains(" = ") && ident_call(m)
        }
        Lang::C => {
            indent == 0
                && (starts_any(t, &["struct ", "enum ", "union ", "typedef ", "class ", "namespace "])
                    || (!starts_any(t, &["#", "/", "*", "}", "{"]) && t.contains('(') && !t.trim_end().ends_with(';') && !t.trim_end().ends_with(',')))
        }
        Lang::Shell => t.starts_with("function ") || (t.contains("()") && t.trim_end().ends_with('{') && ident_call(t)),
    }
}

/// A declaration line, with the line it runs to.
pub(crate) struct Decl {
    /// 0-based index of the declaration line.
    pub line: usize,
    pub indent: usize,
    /// 1-based last line: runs until the next declaration at the same or an outer level.
    pub end: usize,
}

pub(crate) fn declarations(lang: Lang, lines: &[&str]) -> Vec<Decl> {
    let indent_of = |l: &str| l.len() - l.trim_start().len();
    let found: Vec<(usize, usize)> = lines.iter().enumerate().filter(|(_, l)| !l.trim().is_empty() && is_decl(lang, l, indent_of(l))).map(|(i, l)| (i, indent_of(l))).collect();
    found
        .iter()
        .enumerate()
        .map(|(k, &(i, indent))| {
            let mut end = found[k + 1..].iter().find(|(_, d)| *d <= indent).map(|(j, _)| *j).unwrap_or(lines.len());
            while end > i + 1 && lines[end - 1].trim().is_empty() {
                end -= 1;
            }
            Decl { line: i, indent, end }
        })
        .collect()
}

/// The outline of `path` as compact text: `start-end  signature` per
/// declaration, nested ones indented. Err when it can't be read or the
/// language is unknown.
pub fn outline(path: &str) -> Result<String, String> {
    let p = Path::new(path);
    if !p.is_absolute() {
        return Err("pass an absolute path".into());
    }
    let lang = lang(p).ok_or("no outline for this file type: use Grep, then Read with offset/limit")?;
    let text = std::fs::read(p).map_err(|e| format!("{path}: {e}"))?;
    let text = String::from_utf8_lossy(&text);
    let lines: Vec<&str> = text.lines().collect();
    let decls = declarations(lang, &lines);
    let mut out = format!("{path} ({} lines)\n", lines.len());
    for d in decls.iter().take(MAX_SYMBOLS) {
        let sig: String = lines[d.line].trim().chars().take(160).collect();
        out.push_str(&format!("{}{}-{}  {}\n", " ".repeat(d.indent.min(12)), d.line + 1, d.end, sig));
    }
    if decls.len() > MAX_SYMBOLS {
        out.push_str(&format!("… {} more declarations\n", decls.len() - MAX_SYMBOLS));
    }
    if decls.is_empty() {
        out.push_str("no declarations found: use Grep, then Read with offset/limit\n");
    }
    Ok(out)
}

/// Why a whole-file Read of `path` should not happen, if it shouldn't: a
/// source or text file over `READ_WHOLE_MAX_LINES` lines read without
/// offset/limit. Images, PDFs and notebooks pass.
pub fn read_guard(input: &serde_json::Value) -> Option<String> {
    let path = input.get("file_path")?.as_str()?;
    if ["offset", "limit", "pages"].iter().any(|k| input.get(*k).is_some_and(|v| !v.is_null())) {
        return None;
    }
    let ext = Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or("").to_ascii_lowercase();
    if ["png", "jpg", "jpeg", "gif", "webp", "bmp", "pdf", "ipynb"].contains(&ext.as_str()) {
        return None;
    }
    std::fs::metadata(path).ok().filter(|m| m.is_file() && m.len() < 64 << 20)?;
    let lines = std::fs::read(path).ok()?.iter().filter(|b| **b == b'\n').count();
    if lines <= READ_WHOLE_MAX_LINES {
        return None;
    }
    let how = if lang(Path::new(path)).is_some() { "Call the feiyi `outline` tool (mcp__feiyi__outline) for its declarations with line ranges, or Grep for what you need" } else { "Grep for what you need" };
    Some(format!("{path} has {lines} lines; reading it whole fills the context that every later step re-reads. {how}, then Read with offset and limit. To read it whole anyway, pass limit (e.g. limit: 2000)."))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn outlines_rust_and_guards_long_reads() {
        let dir = std::env::temp_dir().join(format!("outline-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("a.rs");
        let body = "use x;\n\npub struct A {\n    b: u32,\n}\n\nimpl A {\n    pub fn new() -> A {\n        A { b: 1 }\n    }\n\n    fn two(&self) {}\n}\n";
        std::fs::write(&f, body).unwrap();
        let o = outline(f.to_str().unwrap()).unwrap();
        assert!(o.contains("3-5  pub struct A {"), "{o}");
        assert!(o.contains("7-13  impl A {"), "{o}");
        assert!(o.contains("    8-10  pub fn new() -> A {"), "{o}");
        assert!(o.contains("    12-13  fn two(&self) {}"), "{o}");
        let p = f.to_str().unwrap();
        assert_eq!(read_guard(&serde_json::json!({ "file_path": p })), None, "short file reads whole");
        std::fs::write(&f, "fn x() {}\n".repeat(READ_WHOLE_MAX_LINES + 1)).unwrap();
        assert!(read_guard(&serde_json::json!({ "file_path": p })).unwrap().contains("outline"));
        assert_eq!(read_guard(&serde_json::json!({ "file_path": p, "limit": 2000 })), None, "explicit limit reads whole");
        assert!(outline("rel/a.rs").is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn script_methods_and_python() {
        assert!(is_decl(Lang::Script, "  async load(id: string) {", 2));
        assert!(!is_decl(Lang::Script, "  if (x) {", 2));
        assert!(is_decl(Lang::Script, "export default function App() {", 0));
        assert!(is_decl(Lang::Python, "    async def run(self):", 4));
        assert!(is_decl(Lang::C, "static int probe(struct device *dev)", 0));
        assert!(!is_decl(Lang::C, "int x(void);", 0));
    }
}
