use proc_macro2::{TokenStream as TokenStream2, TokenTree};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

#[derive(Clone, Debug)]
struct Capture {
    name: String,
    ctype: String,
    mutable: bool,
}

#[derive(Clone)]
struct StmtMacro {
    key: String,
    source: String,
    file: PathBuf,
    line: usize,
    column: usize,
}

#[derive(Default)]
struct Collector {
    includes: Vec<String>,
    cmods: Vec<(String, String)>,
    stmts: Vec<StmtMacro>,
    files: Vec<(PathBuf, String)>,
    file: PathBuf,
}

impl<'ast> Visit<'ast> for Collector {
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        let name = m
            .path
            .segments
            .last()
            .map(|seg| seg.ident.to_string())
            .unwrap_or_default();
        let key = canonical(m.tokens.clone());
        let source = normalize_c(&tokens_to_c(m.tokens.clone()));
        match name.as_str() {
            "cinclude" => self.includes.push(source),
            "cmod" => self.cmods.push((key, source)),
            "c" => {
                if key.contains("iYACI_CAPTURE;")
                    || key.contains("iYACI_CAPTURE_MUT;")
                    || key.contains("iYACI_RETURN;")
                {
                    panic!(
                        "explicit YACI_CAPTURE / YACI_RETURN are no longer supported; \
                         capture types must be inferred by rust-analyzer and return types \
                         must come from the Rust let-binding annotation"
                    );
                }
                let start = m.path.span().start();
                self.stmts.push(StmtMacro {
                    key,
                    source,
                    file: self.file.clone(),
                    line: start.line,
                    column: start.column,
                });
            }
            _ => {}
        }
        visit::visit_macro(self, m);
    }
}

struct Lsp {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: u64,
}

impl Lsp {
    fn new(root: &Path) -> Self {
        let mut child = Command::new("rust-analyzer")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .current_dir(root)
            .spawn()
            .expect("failed to spawn rust-analyzer");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        let mut lsp = Self { child, stdin, stdout, next_id: 0 };
        let root_uri = format!("file://{}", root.display());
        lsp.send(serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": {
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": {},
                "workspaceFolders": [{"uri": root_uri, "name": "root"}]
            }
        }));
        let _ = lsp.wait_for(1);
        lsp.send(serde_json::json!({"jsonrpc":"2.0","method":"initialized","params":{}}));
        lsp
    }

    fn send(&mut self, value: serde_json::Value) {
        let data = serde_json::to_vec(&value).unwrap();
        self.stdin.write_all(b"Content-Length: ").unwrap();
        self.stdin.write_all(data.len().to_string().as_bytes()).unwrap();
        self.stdin.write_all(b"\r\n\r\n").unwrap();
        self.stdin.write_all(&data).unwrap();
        self.stdin.flush().unwrap();
    }

    fn recv(&mut self) -> serde_json::Value {
        let mut len = 0usize;
        loop {
            let mut line = String::new();
            if self.stdout.read_line(&mut line).unwrap() == 0 {
                panic!("rust-analyzer exited unexpectedly");
            }
            if line == "\r\n" {
                break;
            }
            if let Some(v) = line.strip_prefix("Content-Length:") {
                len = v.trim().parse().unwrap();
            }
        }
        let mut buf = vec![0u8; len];
        self.stdout.read_exact(&mut buf).unwrap();
        serde_json::from_slice(&buf).unwrap()
    }

    fn wait_for(&mut self, id: u64) -> serde_json::Value {
        loop {
            let msg = self.recv();
            if msg.get("id").and_then(serde_json::Value::as_u64) == Some(id) {
                return msg.get("result").cloned().unwrap_or(serde_json::Value::Null);
            }
            if let (Some(method), Some(rid)) = (
                msg.get("method").and_then(serde_json::Value::as_str),
                msg.get("id"),
            ) {
                let result = if method == "workspace/configuration" {
                    serde_json::json!([])
                } else {
                    serde_json::Value::Null
                };
                self.send(serde_json::json!({"jsonrpc":"2.0","id":rid,"result":result}));
            }
        }
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.next_id += 1;
        let id = self.next_id;
        self.send(serde_json::json!({
            "jsonrpc":"2.0", "id":id, "method":method, "params":params
        }));
        self.wait_for(id)
    }

    fn did_open(&mut self, path: &Path, text: &str) {
        self.send(serde_json::json!({
            "jsonrpc":"2.0", "method":"textDocument/didOpen",
            "params":{"textDocument":{
                "uri":format!("file://{}", path.display()),
                "languageId":"rust", "version":1, "text":text
            }}
        }));
    }

    fn completion_types(&mut self, path: &Path, line: usize, column: usize) -> HashMap<String, String> {
        let result = self.request("textDocument/completion", serde_json::json!({
            "textDocument":{"uri":format!("file://{}", path.display())},
            "position":{"line":line.saturating_sub(1), "character":column}
        }));
        let items = result.get("items").cloned().unwrap_or(result);
        let mut map = HashMap::new();
        if let Some(arr) = items.as_array() {
            for item in arr {
                if item.get("kind").and_then(serde_json::Value::as_u64) != Some(6) {
                    continue;
                }
                if let (Some(label), Some(detail)) = (
                    item.get("label").and_then(serde_json::Value::as_str),
                    item.get("detail").and_then(serde_json::Value::as_str),
                ) {
                    map.insert(label.to_string(), detail.to_string());
                }
            }
        }
        map
    }
}

impl Drop for Lsp {
    fn drop(&mut self) {
        let _ = self.child.kill();
    }
}

#[derive(Default)]
struct ReturnTypeFinder {
    line: usize,
    column: usize,
    ty: Option<String>,
}

impl<'ast> Visit<'ast> for ReturnTypeFinder {
    fn visit_local(&mut self, local: &'ast syn::Local) {
        let span = local.span();
        let start = span.start();
        let end = span.end();
        let contains = (start.line < self.line
            || (start.line == self.line && start.column <= self.column))
            && (end.line > self.line || (end.line == self.line && end.column >= self.column));
        if contains {
            if let syn::Pat::Type(pat) = &local.pat {
                let ty = &pat.ty;
                self.ty = Some(quote::quote!(#ty).to_string());
            }
        }
        visit::visit_local(self, local);
    }
}

fn infer_return_type(path: &Path, line: usize, column: usize) -> Option<String> {
    let src = fs::read_to_string(path).ok()?;
    let file = syn::parse_file(&src).ok()?;
    let mut finder = ReturnTypeFinder {
        line,
        column,
        ty: None,
    };
    finder.visit_file(&file);
    finder.ty
}

fn rust_type_to_c(t: &str) -> String {
    let t = t.trim();
    match t {
        "i8" => "int8_t".into(),
        "u8" => "uint8_t".into(),
        "i16" => "int16_t".into(),
        "u16" => "uint16_t".into(),
        "i32" => "int32_t".into(),
        "u32" => "uint32_t".into(),
        "i64" => "int64_t".into(),
        "u64" => "uint64_t".into(),
        "isize" => "intptr_t".into(),
        "usize" => "size_t".into(),
        "f32" => "float".into(),
        "f64" => "double".into(),
        "bool" => "bool".into(),
        "char" => "char".into(),
        _ if t.starts_with("&mut ") => format!("{}*", rust_type_to_c(&t[5..])),
        _ if t.starts_with('&') => format!("{}*", rust_type_to_c(&t[1..])),
        _ if t.starts_with("*const ") => format!("const {}*", rust_type_to_c(&t[7..])),
        _ if t.starts_with("*mut ") => format!("{}*", rust_type_to_c(&t[5..])),
        _ => t.to_string(),
    }
}

fn discover_capture_names(body: &str, dir: &Path) -> Vec<(String, bool)> {
    let undeclared = Regex::new(r"use of undeclared identifier '([A-Za-z_]\w*)'").unwrap();
    let mut declared: Vec<String> = Vec::new();
    let mut ast = String::new();
    for _ in 0..8 {
        let decls = declared
            .iter()
            .map(|n| format!("int {};\n", n))
            .collect::<String>();
        let probe = dir.join("__yaci_names.c");
        fs::write(
            &probe,
            format!(
                "#include \"yaci_includes.h\"\n#include \"yaci_probe_prelude.h\"\n#include \"yaci_cmod_decls.h\"\nint __yaci_names(void) {{\n{}{}\nreturn 0;\n}}\n",
                decls, body
            ),
        )
        .unwrap();
        let output = Command::new(clang())
            .arg("-Xclang")
            .arg("-ast-dump")
            .arg("-Xclang")
            .arg("-ast-dump-filter=__yaci_names")
            .arg("-fsyntax-only")
            .arg("-ferror-limit=0")
            .arg("-I")
            .arg(dir.parent().unwrap())
            .arg(&probe)
            .output()
            .expect("failed to run clang for capture discovery");
        let stderr = String::from_utf8_lossy(&output.stderr);
        let mut fresh = Vec::new();
        for caps in undeclared.captures_iter(&stderr) {
            let name = caps[1].to_string();
            let is_call = Regex::new(&format!(r"{}\s*\(", regex::escape(&name)))
                .unwrap()
                .is_match(body);
            if !declared.contains(&name) && !is_call && !fresh.contains(&name) {
                fresh.push(name);
            }
        }
        if fresh.is_empty() {
            ast = String::from_utf8_lossy(&output.stdout).to_string();
            break;
        }
        declared.extend(fresh);
    }

    let lines: Vec<&str> = ast.lines().collect();
    let mut result = Vec::new();
    for name in declared {
        let mut mutable = false;
        for (i, line) in lines.iter().enumerate() {
            if !line.contains("DeclRefExpr") || !line.contains(&format!("'{}'", name)) {
                continue;
            }
            let start = i.saturating_sub(5);
            let end = (i + 6).min(lines.len());
            let ctx = lines[start..end].join("\n");
            let assigned = ctx.contains("BinaryOperator")
                && ["'='", "'+='", "'-='", "'*='", "'/='", "'%='"]
                    .iter()
                    .any(|op| ctx.contains(op));
            let incremented = ctx.contains("UnaryOperator")
                && (ctx.contains("'++'") || ctx.contains("'--'"));
            if assigned || incremented {
                mutable = true;
            }
        }
        result.push((name, mutable));
    }
    result
}

fn augment_body(body: &str, names: &[(String, bool)], types: &HashMap<String, String>) -> String {
    let mut decls = String::new();
    for (name, mutable) in names {
        let ty = types.get(name).unwrap_or_else(|| {
            panic!(
                "rust-analyzer cannot resolve the type of captured variable `{}`; \
                 generate any Rust bindings it uses before calling ya_c_inline_build::setup()",
                name
            )
        });
        let c_ty = rust_type_to_c(ty);
        let macro_name = if *mutable { "YACI_CAPTURE_MUT" } else { "YACI_CAPTURE" };
        decls.push_str(&format!("{}({}, {});\n", macro_name, c_ty, name));
    }
    format!("{}{}\n", decls, body)
}

pub struct Builder {
    always_inline: bool,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    pub fn new() -> Self {
        Self { always_inline: false }
    }

    /// 启用实验性 always-inline：把 c! 桩函数编译为 LLVM bitcode 并交给 LTO。
    /// 这是可选优化；关闭时 c! 仍然通过普通静态库调用正常工作。
    pub fn always_inline(mut self, enabled: bool) -> Self {
        self.always_inline = enabled;
        self
    }

    pub fn setup(self) {
        setup_with(self.always_inline);
    }
}

pub fn setup() {
    Builder::new().setup();
}

fn setup_with(always_inline: bool) {
    let manifest = PathBuf::from(
        env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR is set by cargo"),
    );
    let out = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));
    let yaci = out.join("yaci");
    let mods = yaci.join("mods");
    let stmts = yaci.join("stmts");
    fs::create_dir_all(&mods).unwrap();
    fs::create_dir_all(&stmts).unwrap();

    println!("cargo:rustc-link-search=native={}", yaci.display());
    println!("cargo:rerun-if-changed={}", manifest.join("src").display());
    println!("cargo:rerun-if-env-changed=YA_C_INLINE_CLANG");

    let mut collector = Collector::default();
    collect_macros(&manifest.join("src"), &mut collector);

    let includes = collector.includes.join("\n");
    fs::write(
        yaci.join("yaci_includes.h"),
        format!(
            "#pragma once\n#include <stdint.h>\n#include <stddef.h>\n#include <stdbool.h>\n{}\n",
            includes
        ),
    )
    .unwrap();

    fs::write(
        yaci.join("yaci_probe_prelude.h"),
        concat!(
            "#pragma once\n",
            "#define YACI_CAPTURE(type, name) type name __attribute__((annotate(\"yaci_capture:\" #name \":const\")))\n",
            "#define YACI_CAPTURE_MUT(type, name) type name __attribute__((annotate(\"yaci_capture:\" #name \":mut\")))\n",
            "#define YACI_RETURN(type) typedef type yaci_return_type __attribute__((annotate(\"yaci_return\")))\n",
        ),
    )
    .unwrap();
    fs::write(
        yaci.join("yaci_final_prelude.h"),
        concat!(
            "#pragma once\n",
            "#define YACI_CAPTURE(type, name)\n",
            "#define YACI_CAPTURE_MUT(type, name)\n",
            "#define YACI_RETURN(type)\n",
        ),
    )
    .unwrap();

    let cmods = dedup(collector.cmods);
    let mut declarations = String::from("#pragma once\n");
    for (key, body) in &cmods {
        let h = hash(key);
        let csrc = mods.join(format!("{}.c", h));
        let source = format!("#define YACI_CMOD_IMPL\n#include \"yaci_includes.h\"\n{body}\n");
        fs::write(&csrc, &source).unwrap();
        declarations.push_str(&cmod_declarations(&yaci, &csrc, &source));
        let lib = format!("yaci_m_{}", h);
        compile_static(&yaci, &lib, &csrc);
        println!("cargo:rustc-link-lib=static:+whole-archive={}", lib);
    }
    fs::write(yaci.join("yaci_cmod_decls.h"), declarations).unwrap();

    let stmt_macros = dedup_stmts(collector.stmts);
    let mut lsp = None;
    for stmt in &stmt_macros {
        let h = hash(&stmt.key);
        let names = discover_capture_names(&stmt.source, &stmts);
        let mut body = if names.is_empty() {
            stmt.source.clone()
        } else {
            let lsp = lsp.get_or_insert_with(|| {
                let mut lsp = Lsp::new(&manifest);
                for (path, text) in &collector.files {
                    lsp.did_open(path, text);
                }
                std::thread::sleep(std::time::Duration::from_millis(300));
                lsp
            });
            let types = lsp.completion_types(&stmt.file, stmt.line, stmt.column);
            augment_body(&stmt.source, &names, &types)
        };
        let ret_ty = infer_return_type(&stmt.file, stmt.line, stmt.column);
        if let Some(ty) = ret_ty {
            body = format!("YACI_RETURN({});\n{}", rust_type_to_c(&ty), body);
        } else if body.contains("return") {
            panic!(
                "cannot infer c! return type; add an explicit Rust type annotation \
                 to the let-binding and rebuild"
            );
        }
        generate_stmt(&yaci, &stmts, &h, &body, always_inline);
    }
}

fn dedup_stmts(v: Vec<StmtMacro>) -> Vec<StmtMacro> {
    let mut seen = HashSet::new();
    v.into_iter().filter(|s| seen.insert(s.key.clone())).collect()
}

fn collect_macros(dir: &Path, collector: &mut Collector) {
    if !dir.is_dir() {
        return;
    }
    let mut paths = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_macros(&path, collector);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            let src = fs::read_to_string(&path).unwrap();
            let file = syn::parse_file(&src)
                .unwrap_or_else(|e| panic!("failed to parse {}: {}", path.display(), e));
            collector.files.push((path.clone(), src));
            collector.file = path.clone();
            collector.visit_file(&file);
        }
    }
}

fn dedup(v: Vec<(String, String)>) -> Vec<(String, String)> {
    let mut seen = std::collections::HashSet::new();
    v.into_iter().filter(|s| seen.insert(s.0.clone())).collect()
}

fn canonical(ts: TokenStream2) -> String {
    let mut s = String::new();
    for tt in ts {
        match tt {
            TokenTree::Ident(id) => s.push_str(&format!("i{};", id)),
            TokenTree::Literal(lit) => s.push_str(&format!("l{};", lit)),
            TokenTree::Punct(p) => {
                let spacing = if p.spacing() == proc_macro2::Spacing::Joint { 'j' } else { 'a' };
                s.push_str(&format!("p{}{};", p.as_char(), spacing));
            }
            TokenTree::Group(g) => {
                let d = match g.delimiter() {
                    proc_macro2::Delimiter::Brace => 'b',
                    proc_macro2::Delimiter::Parenthesis => 'p',
                    proc_macro2::Delimiter::Bracket => 'k',
                    proc_macro2::Delimiter::None => 'n',
                };
                s.push_str(&format!("g{}({});", d, canonical(g.stream())));
            }
        }
    }
    s
}

fn hash(s: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h)
}

fn tokens_to_c(ts: TokenStream2) -> String {
    fn is_word(c: char) -> bool {
        c.is_alphanumeric() || c == '_'
    }
    fn newline_to(line: usize, current: &mut usize, out: &mut String) {
        if line > *current {
            for _ in 0..(line - *current) {
                out.push('\n');
            }
            *current = line;
        }
    }

    let mut out = String::new();
    let mut line = 1usize;
    for tt in ts {
        let start_line = tt.span().start().line;
        let end_line = tt.span().end().line;
        newline_to(start_line, &mut line, &mut out);
        match tt {
            TokenTree::Ident(id) => {
                if out.chars().last().map(is_word).unwrap_or(false) {
                    out.push(' ');
                }
                out.push_str(&id.to_string());
            }
            TokenTree::Literal(lit) => {
                if out.chars().last().map(is_word).unwrap_or(false) {
                    out.push(' ');
                }
                out.push_str(&lit.to_string());
            }
            TokenTree::Punct(p) => {
                let c = p.as_char();
                match c {
                    '#' | '.' | '<' | '>' => {
                        while out.ends_with(' ') {
                            out.pop();
                        }
                        out.push(c);
                    }
                    _ => {
                        out.push(c);
                        if p.spacing() == proc_macro2::Spacing::Alone {
                            out.push(' ');
                        }
                    }
                }
            }
            TokenTree::Group(g) => {
                let (open, close) = match g.delimiter() {
                    proc_macro2::Delimiter::Brace => ('{', '}'),
                    proc_macro2::Delimiter::Parenthesis => ('(', ')'),
                    proc_macro2::Delimiter::Bracket => ('[', ']'),
                    proc_macro2::Delimiter::None => (' ', ' '),
                };
                if out.chars().last().map(is_word).unwrap_or(false) {
                    out.push(' ');
                }
                out.push(open);
                newline_to(g.span().start().line, &mut line, &mut out);
                let inner = tokens_to_c(g.stream());
                if !inner.is_empty() {
                    out.push_str(&inner);
                }
                newline_to(g.span().end().line, &mut line, &mut out);
                out.push(close);
            }
        }
        line = line.max(end_line);
    }
    out
}

/// proc_macro2 的 to_string 会丢掉换行；在预处理指令前补回换行。
fn normalize_c(src: &str) -> String {
    let mut out = String::new();
    let mut in_str = false;
    let mut in_char = false;
    let mut escaped = false;
    for ch in src.chars() {
        if in_str {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '"' {
                in_str = false;
            }
            continue;
        }
        if in_char {
            out.push(ch);
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == '\'' {
                in_char = false;
            }
            continue;
        }
        match ch {
            '"' => {
                in_str = true;
                out.push(ch);
            }
            '\'' => {
                in_char = true;
                out.push(ch);
            }
            '#' => {
                while out.ends_with(' ') || out.ends_with('\t') {
                    out.pop();
                }
                if !out.ends_with('\n') && !out.is_empty() {
                    out.push('\n');
                }
                out.push('#');
            }
            _ => out.push(ch),
        }
    }
    out.push('\n');
    out
}

fn clang() -> String {
    env::var("YA_C_INLINE_CLANG").unwrap_or_else(|_| "clang".to_string())
}

fn run(mut cmd: Command, what: &str) -> Output {
    let output = cmd.output().unwrap_or_else(|e| panic!("{}: {}", what, e));
    if !output.status.success() {
        panic!(
            "{} failed:\nstdout:\n{}\nstderr:\n{}",
            what,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
    }
    output
}

fn cmod_declarations(dir: &Path, csrc: &Path, source: &str) -> String {
    let mut cmd = Command::new(clang());
    cmd.args(["-Xclang", "-ast-dump=json", "-fsyntax-only", "-I"])
        .arg(dir)
        .arg(csrc);
    let output = run(cmd, "clang AST dump for cmod!");
    let ast: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let items = ast["inner"].as_array().unwrap();
    let private_functions: HashSet<_> = items
        .iter()
        .filter(|item| item["kind"] == "FunctionDecl" && item["storageClass"] == "static")
        .filter_map(|item| item["name"].as_str())
        .collect();
    let mut declarations = String::new();
    for item in items.iter().filter(|item| item["kind"] == "FunctionDecl") {
        let name = item["name"].as_str().unwrap();
        let loc = item["loc"].get("expansionLoc").unwrap_or(&item["loc"]);
        if private_functions.contains(name) || loc["offset"].is_null() || !loc["includedFrom"].is_null() {
            continue;
        }
        let Some(body) = item["inner"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|child| child["kind"] == "CompoundStmt")
        else {
            continue;
        };
        let start = item["range"]["begin"]["offset"].as_u64();
        let end = body["range"]["begin"]["offset"].as_u64();
        if let (Some(start), Some(end)) = (start, end) {
            declarations.push_str(source[start as usize..end as usize].trim_end());
            declarations.push_str(";\n");
        } else {
            let ty = item["type"]["qualType"].as_str().unwrap();
            declarations.push_str(&format!("extern __typeof__({ty}) {name};\n"));
        }
    }
    declarations
}

fn compile_static(dir: &Path, lib: &str, csrc: &Path) {
    let obj = dir.join(format!("{}.o", lib));
    run(
        {
            let mut cmd = Command::new(clang());
            cmd.arg("-O2")
                .arg("-fPIC")
                .arg("-c")
                .arg(csrc)
                .arg("-I")
                .arg(dir)
                .arg("-o")
                .arg(&obj);
            cmd
        },
        "clang -c",
    );
    let archive = dir.join(format!("lib{}.a", lib));
    run(
        {
            let mut cmd = Command::new("ar");
            cmd.arg("rcs").arg(&archive).arg(&obj);
            cmd
        },
        "ar rcs",
    );
}

fn compile_bitcode(dir: &Path, name: &str, csrc: &Path) -> PathBuf {
    let bc = dir.join(format!("{}.bc", name));
    run(
        {
            let mut cmd = Command::new(clang());
            cmd.arg("-O2")
                .arg("-flto")
                .arg("-emit-llvm")
                .arg("-c")
                .arg(csrc)
                .arg("-I")
                .arg(dir)
                .arg("-o")
                .arg(&bc);
            cmd
        },
        "clang -flto -emit-llvm -c",
    );
    bc
}

fn tool(name: &str, env_name: &str) -> String {
    env::var(env_name).unwrap_or_else(|_| name.to_string())
}

fn compile_inlined_wrapper(
    yaci: &Path,
    stmts: &Path,
    h: &str,
    csrc: &Path,
    rust_params: &[String],
    ret_rust: &str,
) -> PathBuf {
    let c_bc = compile_bitcode(yaci, &format!("yaci_s_{}", h), csrc);
    let names = rust_params
        .iter()
        .map(|p| p.split(':').next().unwrap().trim().to_string())
        .collect::<Vec<_>>();
    let symbol = format!("__yaci_stmt_{}", h);
    let rust_symbol = format!("__yaci_rust_{}", h);
    let wrapper_rs = stmts.join(format!("{}_wrapper.rs", h));
    fs::write(
        &wrapper_rs,
        format!(
            "unsafe extern \"C\" {{\n    fn {}({}) -> {};\n}}\n#[no_mangle]\npub extern \"C\" fn {}({}) -> {} {{\n    unsafe {{ {}({}) }}\n}}\n",
            symbol,
            rust_params.join(", "),
            ret_rust,
            rust_symbol,
            rust_params.join(", "),
            ret_rust,
            symbol,
            names.join(", ")
        ),
    )
    .unwrap();

    let rust_bc = stmts.join(format!("{}_wrapper.bc", h));
    let mut rustc = Command::new("rustc");
    rustc
        .arg("--crate-type")
        .arg("lib")
        .arg("--emit=llvm-bc")
        .arg("-C")
        .arg("opt-level=2")
        .arg("-A")
        .arg("warnings")
        .arg(&wrapper_rs)
        .arg("-o")
        .arg(&rust_bc);
    if let Ok(target) = env::var("TARGET") {
        rustc.arg("--target").arg(target);
    }
    run(rustc, "rustc --emit=llvm-bc");

    let linked = stmts.join(format!("{}_linked.bc", h));
    run(
        {
            let mut cmd = Command::new(tool("llvm-link", "YA_C_INLINE_LLVM_LINK"));
            cmd.arg(&rust_bc).arg(&c_bc).arg("-o").arg(&linked);
            cmd
        },
        "llvm-link",
    );
    let inlined = stmts.join(format!("{}_inlined.bc", h));
    run(
        {
            let mut cmd = Command::new(tool("opt", "YA_C_INLINE_OPT"));
            cmd.arg("-passes=always-inline,globaldce")
                .arg(&linked)
                .arg("-o")
                .arg(&inlined);
            cmd
        },
        "opt -passes=always-inline",
    );
    let object = stmts.join(format!("{}_wrapper.o", h));
    run(
        {
            let mut cmd = Command::new(tool("llc", "YA_C_INLINE_LLC"));
            cmd.arg("-filetype=obj")
                .arg("-O2")
                .arg(&inlined)
                .arg("-o")
                .arg(&object);
            cmd
        },
        "llc -filetype=obj",
    );
    object
}

fn discover(probe: &Path, dir: &Path) -> (Vec<Capture>, Option<String>) {
    let mut cmd = Command::new(clang());
    cmd.arg("-Xclang")
        .arg("-ast-dump")
        .arg("-Xclang")
        .arg("-ast-dump-filter=__yaci_probe")
        .arg("-fsyntax-only")
        .arg("-ferror-limit=0")
        .arg("-I")
        .arg(dir)
        .arg(probe);
    let output = cmd.output().expect("failed to run clang AST dump");
    let ast = String::from_utf8_lossy(&output.stdout);

    let var_re = Regex::new(r"VarDecl\b[^\n]*?\b([A-Za-z_]\w*)\s+'([^']+)'").unwrap();
    let ann_re = Regex::new(r#"AnnotateAttr\b[^\n]*"yaci_capture:[^"]*:(const|mut)""#).unwrap();
    let ret_re = Regex::new(r"TypedefDecl\b[^\n]*\byaci_return_type\s+'([^']+)'").unwrap();

    let mut captures = Vec::new();
    let mut pending: Option<(String, String)> = None;
    let mut ret = None;
    for line in ast.lines() {
        if let Some(caps) = var_re.captures(line) {
            pending = Some((caps[1].to_string(), caps[2].to_string()));
            continue;
        }
        if let Some(caps) = ann_re.captures(line) {
            if let Some((name, ctype)) = pending.take() {
                captures.push(Capture {
                    name,
                    ctype,
                    mutable: &caps[1] == "mut",
                });
            }
            continue;
        }
        if let Some(caps) = ret_re.captures(line) {
            ret = Some(caps[1].to_string());
        }
    }
    (captures, ret)
}

fn c_type_to_rust(t: &str) -> String {
    let raw = t.trim();
    let const_ptr = raw.starts_with("const ");
    let t = raw
        .replace("const ", "")
        .replace("volatile ", "")
        .replace("struct ", "")
        .trim()
        .to_string();
    if let Some(base) = t.strip_suffix('*') {
        let base = c_type_to_rust(base.trim());
        return if const_ptr {
            format!("*const {}", base)
        } else {
            format!("*mut {}", base)
        };
    }
    let mapped = match t.as_str() {
        "void" => "()".to_string(),
        "bool" | "_Bool" => "bool".to_string(),
        "char" => "std::ffi::c_char".to_string(),
        "signed char" => "std::ffi::c_schar".to_string(),
        "unsigned char" => "std::ffi::c_uchar".to_string(),
        "short" | "short int" | "signed short" | "signed short int" => {
            "std::ffi::c_short".to_string()
        }
        "unsigned short" | "unsigned short int" => "std::ffi::c_ushort".to_string(),
        "int" | "signed" | "signed int" => "std::ffi::c_int".to_string(),
        "unsigned" | "unsigned int" => "std::ffi::c_uint".to_string(),
        "long" | "long int" | "signed long" | "signed long int" => {
            "std::ffi::c_long".to_string()
        }
        "unsigned long" | "unsigned long int" => "std::ffi::c_ulong".to_string(),
        "long long" | "long long int" | "signed long long" | "signed long long int" => {
            "std::ffi::c_longlong".to_string()
        }
        "unsigned long long" | "unsigned long long int" => {
            "std::ffi::c_ulonglong".to_string()
        }
        "float" => "f32".to_string(),
        "double" => "f64".to_string(),
        "int8_t" => "i8".to_string(),
        "uint8_t" => "u8".to_string(),
        "int16_t" => "i16".to_string(),
        "uint16_t" => "u16".to_string(),
        "int32_t" => "i32".to_string(),
        "uint32_t" => "u32".to_string(),
        "int64_t" => "i64".to_string(),
        "uint64_t" => "u64".to_string(),
        "intptr_t" => "isize".to_string(),
        "uintptr_t" | "size_t" => "usize".to_string(),
        "ssize_t" => "isize".to_string(),
        _ => t.to_string(),
    };
    mapped
}

fn generate_stmt(yaci: &Path, stmts: &Path, h: &str, body: &str, always_inline: bool) {
    let probe = stmts.join(format!("{}_probe.c", h));
    fs::write(
        &probe,
        format!(
            "#include \"yaci_includes.h\"\n#include \"yaci_probe_prelude.h\"\n#include \"yaci_cmod_decls.h\"\nint __yaci_probe(void) {{\n{}\nreturn 0;\n}}\n",
            body
        ),
    )
    .unwrap();
    let (captures, ret) = discover(&probe, yaci);
    let ret_c = ret.as_deref().unwrap_or("void");
    let ret_rust = c_type_to_rust(ret_c);

    let mut params = Vec::new();
    let mut rust_params = Vec::new();
    let mut args = Vec::new();
    let mut defs = String::new();
    let mut undefs = String::new();
    for (i, cap) in captures.iter().enumerate() {
        let base = cap.ctype.trim_start_matches("const ").trim();
        if cap.mutable {
            params.push(format!("{} *p{}", base, i));
            rust_params.push(format!("p{}: *mut ()", i));
            args.push(format!("&raw mut {} as *mut ()", cap.name));
        } else {
            params.push(format!("const {} *p{}", base, i));
            rust_params.push(format!("p{}: *const ()", i));
            args.push(format!("&raw const {} as *const ()", cap.name));
        }
        defs.push_str(&format!("#define {} (*p{})\n", cap.name, i));
        undefs.push_str(&format!("#undef {}\n", cap.name));
    }

    let csrc = stmts.join(format!("{}.c", h));
    let attr = if always_inline {
        "__attribute__((always_inline)) "
    } else {
        ""
    };
    fs::write(
        &csrc,
        format!(
            "#include \"yaci_includes.h\"\n#include \"yaci_final_prelude.h\"\n#include \"yaci_cmod_decls.h\"\n{}{} __yaci_stmt_{}({}) {{\n{}{}{}}}\n",
            attr,
            ret_c,
            h,
            params.join(", "),
            defs,
            body,
            undefs
        ),
    )
    .unwrap();

    let lib = format!("yaci_s_{}", h);
    let (link_attr, symbol) = if always_inline {
        let object = compile_inlined_wrapper(yaci, stmts, h, &csrc, &rust_params, &ret_rust);
        println!("cargo:rustc-link-arg={}", object.display());
        (String::new(), format!("__yaci_rust_{}", h))
    } else {
        compile_static(yaci, &lib, &csrc);
        println!("cargo:rustc-link-lib=static={}", lib);
        (
            format!("    #[link(name = \"{}\", kind = \"static\")]\n", lib),
            format!("__yaci_stmt_{}", h),
        )
    };
    fs::write(
        stmts.join(format!("{}.rs", h)),
        format!(
            "{{\n{}    unsafe extern \"C\" {{\n        fn {}({}) -> {};\n    }}\n    {}({})\n}}\n",
            link_attr,
            symbol,
            rust_params.join(", "),
            ret_rust,
            symbol,
            args.join(", ")
        ),
    )
    .unwrap();
}
