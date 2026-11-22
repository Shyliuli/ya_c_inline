use proc_macro2::{Delimiter, TokenStream as TokenStream2, TokenTree};
use regex::Regex;
use std::collections::{HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

#[derive(Clone, Debug)]
struct Capture {
    name: String,
    ctype: String,
    mutable: bool,
}

#[derive(Clone, Debug)]
struct Signature {
    captures: Vec<(String, String)>,
    ret: Option<String>,
}

#[derive(Clone)]
struct StmtMacro {
    key: String,
    source: String,
    file: PathBuf,
    line: usize,
    column: usize,
    sig: Signature,
}

#[derive(Default)]
struct Collector {
    includes: Vec<String>,
    cmods: Vec<(String, String)>,
    stmts: Vec<StmtMacro>,
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
        let (sig, body_tokens) = if name == "c" {
            let (sig, body) = parse_signature(m.tokens.clone());
            (sig, body)
        } else {
            (
                Signature {
                    captures: Vec::new(),
                    ret: None,
                },
                m.tokens.clone(),
            )
        };
        let mut source = String::new();
        let mut line = body_tokens.span().start().line;
        tokens_to_c(body_tokens, &mut line, &mut source);
        let source = normalize_c(&source);
        match name.as_str() {
            "cinclude" => self.includes.push(source),
            "cmod" => self.cmods.push((key, source)),
            "c" => {
                let start = m.path.span().start();
                self.stmts.push(StmtMacro {
                    key,
                    source,
                    file: self.file.clone(),
                    line: start.line,
                    column: start.column,
                    sig,
                });
            }
            _ => {}
        }
        visit::visit_macro(self, m);
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

/// 收集文件内所有 `let name: Type = ...` 的类型标注，作为捕获类型的 fallback。
fn infer_let_types(path: &Path) -> HashMap<String, String> {
    #[derive(Default)]
    struct Finder {
        types: HashMap<String, String>,
    }
    impl<'ast> Visit<'ast> for Finder {
        fn visit_local(&mut self, local: &'ast syn::Local) {
            if let syn::Pat::Type(pat) = &local.pat {
                let name = match &*pat.pat {
                    syn::Pat::Ident(id) => Some(id.ident.to_string()),
                    _ => None,
                };
                if let Some(name) = name {
                    let ty = &pat.ty;
                    self.types
                        .entry(name)
                        .or_insert_with(|| quote::quote!(#ty).to_string());
                }
            }
            visit::visit_local(self, local);
        }
    }

    let Ok(src) = fs::read_to_string(path) else {
        return HashMap::new();
    };
    let Ok(file) = syn::parse_file(&src) else {
        return HashMap::new();
    };
    let mut finder = Finder::default();
    finder.visit_file(&file);
    finder.types
}

/// 解析 `c!` 宏体开头的可选签名：`(name: RustType, ...) -> RustType`
///
/// 签名必须先于 C 语句；两部分各自可选。返回去掉签名后的 C token。
fn parse_signature(ts: TokenStream2) -> (Signature, TokenStream2) {
    let tokens: Vec<TokenTree> = ts.into_iter().collect();
    let mut i = 0usize;
    let mut captures = Vec::new();

    if matches!(tokens.get(i), Some(TokenTree::Group(g)) if g.delimiter() == Delimiter::Parenthesis) {
        let TokenTree::Group(g) = &tokens[i] else { unreachable!() };
        for part in split_commas(&g.stream()) {
            let colon = part
                .iter()
                .position(|t| matches!(t, TokenTree::Punct(p) if p.as_char() == ':'))
                .unwrap_or_else(|| panic!("c! 捕获标注必须写成 `name: RustType`"));
            let name = match &part[..colon] {
                [TokenTree::Ident(id)] => id.to_string(),
                _ => panic!("c! 捕获标注的变量名必须是单个标识符"),
            };
            let ty_tokens: TokenStream2 = part[colon + 1..].iter().cloned().collect();
            let ty: syn::Type = syn::parse2(ty_tokens)
                .unwrap_or_else(|e| panic!("c! 捕获 `{name}` 的类型不是合法 Rust 类型: {e}"));
            captures.push((name, quote::quote!(#ty).to_string()));
        }
        i += 1;
    }

    let mut ret = None;
    if matches!(
        (tokens.get(i), tokens.get(i + 1)),
        (Some(TokenTree::Punct(a)), Some(TokenTree::Punct(b)))
            if a.as_char() == '-' && b.as_char() == '>'
    ) {
        let start = i + 2;
        let mut found = None;
        for end in (start + 1..=tokens.len()).rev() {
            let candidate: TokenStream2 = tokens[start..end].iter().cloned().collect();
            if let Ok(ty) = syn::parse2::<syn::Type>(candidate) {
                found = Some((end, quote::quote!(#ty).to_string()));
                break;
            }
        }
        let (end, ty) = found.unwrap_or_else(|| panic!("`->` 后必须是合法的 Rust 返回类型"));
        ret = Some(ty);
        i = end;
    }

    let body = tokens[i..].iter().cloned().collect();
    (
        Signature {
            captures,
            ret,
        },
        body,
    )
}

/// 按顶层逗号切分 token（忽略 Group 内部与 `<...>` 里的逗号）。
fn split_commas(ts: &TokenStream2) -> Vec<Vec<TokenTree>> {
    let mut parts = vec![Vec::new()];
    let mut angle = 0i32;
    for tt in ts.clone() {
        match &tt {
            TokenTree::Group(_) => parts.last_mut().unwrap().push(tt),
            TokenTree::Punct(p) if p.as_char() == '<' => {
                angle += 1;
                parts.last_mut().unwrap().push(tt);
            }
            TokenTree::Punct(p) if p.as_char() == '>' => {
                angle = (angle - 1).max(0);
                parts.last_mut().unwrap().push(tt);
            }
            TokenTree::Punct(p) if p.as_char() == ',' && angle == 0 => {
                parts.push(Vec::new());
            }
            _ => parts.last_mut().unwrap().push(tt),
        }
    }
    parts.retain(|p| !p.is_empty());
    parts
}

fn rust_type_to_c(t: &str) -> String {
    let t = t
        .trim()
        .replace("* mut ", "*mut ")
        .replace("* const ", "*const ")
        .replace("& mut ", "&mut ")
        .replace("& ", "&");
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

fn capture_written(node: &serde_json::Value, id: &str, write: bool) -> bool {
    let write_operand = match node["kind"].as_str() {
        Some("BinaryOperator") => node["opcode"] == "=",
        Some("CompoundAssignOperator") => true,
        Some("UnaryOperator") => matches!(node["opcode"].as_str(), Some("++" | "--" | "&")),
        Some("ParenExpr") => write,
        Some("MemberExpr") => write && node["isArrow"] == false,
        Some("DeclRefExpr") => return write && node["referencedDecl"]["id"] == id,
        _ => false,
    };
    node["inner"].as_array().into_iter().flatten().enumerate()
        .any(|(index, child)| capture_written(child, id, write_operand && index == 0))
}

fn discover_capture_names(body: &str, dir: &Path, types: &HashMap<String, String>) -> Vec<(String, bool)> {
    let undeclared = Regex::new(r"use of undeclared identifier '([A-Za-z_]\w*)'").unwrap();
    let mut declared: Vec<String> = Vec::new();
    let mut ast = String::new();
    for _ in 0..8 {
        let decls = declared
            .iter()
            .map(|name| format!("{} {name};\n", rust_type_to_c(types.get(name).map_or("i32", String::as_str))))
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
            .arg("-ast-dump=json")
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
            if !declared.contains(&name) && !fresh.contains(&name) {
                fresh.push(name);
            }
        }
        if fresh.is_empty() {
            ast = String::from_utf8_lossy(&output.stdout).to_string();
            break;
        }
        declared.extend(fresh);
    }

    let ast: serde_json::Value = serde_json::from_str(&ast).expect("invalid Clang capture AST");
    let mut result = Vec::new();
    let statements = ast["inner"][0]["inner"].as_array().unwrap();
    for (name, decl) in declared.into_iter().zip(statements) {
        let mutable = capture_written(&ast, decl["inner"][0]["id"].as_str().unwrap(), false);
        result.push((name, mutable));
    }
    result
}

fn augment_body(body: &str, names: &[(String, bool)], types: &HashMap<String, String>) -> String {
    let mut decls = String::new();
    for (name, mutable) in names {
        let ty = types.get(name).unwrap_or_else(|| {
            panic!(
                "cannot resolve the type of captured variable `{}`; write \
                 `c!{{ ({}: RustType) ... }}` or add a Rust `let` type annotation",
                name, name
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
    write_if_changed(
        &yaci.join("yaci_includes.h"),
        &format!(
            "#pragma once\n#include <stdint.h>\n#include <stddef.h>\n#include <stdbool.h>\n{}\n",
            includes
        ),
    );

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
    write_if_changed(
        &yaci.join("yaci_final_prelude.h"),
        concat!(
            "#pragma once\n",
            "#define YACI_CAPTURE(type, name)\n",
            "#define YACI_CAPTURE_MUT(type, name)\n",
            "#define YACI_RETURN(type)\n",
        ),
    );

    let cmods = dedup(collector.cmods);
    let mut declarations = String::from("#pragma once\n");
    for (key, body) in &cmods {
        let h = hash(key);
        let csrc = mods.join(format!("{}.c", h));
        let source = format!("#define YACI_CMOD_IMPL\n#include \"yaci_includes.h\"\n{body}\n");
        write_if_changed(&csrc, &source);
        declarations.push_str(&cmod_declarations(&yaci, &csrc, &source));
        let lib = format!("yaci_m_{}", h);
        compile_static(&yaci, &lib, &csrc);
        println!("cargo:rustc-link-lib=static:+whole-archive={}", lib);
    }
    write_if_changed(&yaci.join("yaci_cmod_decls.h"), &declarations);

    let stmt_macros = dedup_stmts(collector.stmts);
    for stmt in &stmt_macros {
        let h = hash(&stmt.key);
        let mut body = {
            // 优先级：显式签名 > syn 解析 let 标注 > 报错
            let mut types: HashMap<String, String> = stmt
                .sig
                .captures
                .iter()
                .cloned()
                .collect();
            let let_types = infer_let_types(&stmt.file);
            types = let_types.into_iter().chain(types).collect();
            let names = discover_capture_names(&stmt.source, &stmts, &types);
            augment_body(&stmt.source, &names, &types)
        };
        // 返回类型：显式签名 > let 标注 > 报错
        let ret_ty = stmt
            .sig
            .ret
            .clone()
            .or_else(|| infer_return_type(&stmt.file, stmt.line, stmt.column));
        if let Some(ty) = ret_ty {
            body = format!("YACI_RETURN({});\n{}", rust_type_to_c(&ty), body);
        } else if body.contains("return") {
            panic!(
                "cannot infer c! return type; write `c!{{ -> Type ... }}` or add an \
                 explicit Rust type annotation to the let-binding"
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

fn write_if_changed(path: &Path, contents: &str) {
    if fs::read(path).ok().as_deref() != Some(contents.as_bytes()) {
        fs::write(path, contents).unwrap();
    }
}

fn is_fresh(output: &Path, inputs: &[&Path], depfile: &Path) -> bool {
    if !depfile.exists() {
        return false;
    }
    let Ok(output_time) = fs::metadata(output).and_then(|metadata| metadata.modified()) else {
        return false;
    };
    let dependencies = dependencies(depfile);
    let mut fresh = true;
    for path in inputs
        .iter()
        .copied()
        .chain(dependencies.iter().map(PathBuf::as_path))
    {
        println!("cargo:rerun-if-changed={}", path.display());
        fresh &= fs::metadata(path)
            .and_then(|metadata| metadata.modified())
            .is_ok_and(|modified| modified <= output_time);
    }
    fresh
}

fn dependencies(depfile: &Path) -> Vec<PathBuf> {
    let Ok(contents) = fs::read_to_string(depfile) else {
        return Vec::new();
    };
    let contents = contents.replace("\\\n", "").replace("\\ ", "\0");
    let Some((_, paths)) = contents.split_once(':') else {
        return Vec::new();
    };
    paths
        .split_whitespace()
        .map(|path| PathBuf::from(path.replace('\0', " ")))
        .collect()
}

fn tokens_to_c(ts: TokenStream2, line: &mut usize, out: &mut String) {
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

    for tt in ts {
        let start_line = tt.span().start().line;
        let end_line = tt.span().end().line;
        newline_to(start_line, line, out);
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
                tokens_to_c(g.stream(), line, out);
                newline_to(end_line, line, out);
                out.push(close);
            }
        }
        *line = (*line).max(end_line);
    }
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
    let archive = dir.join(format!("lib{}.a", lib));
    let depfile = dir.join(format!("{}.d", lib));
    if is_fresh(&archive, &[csrc, &obj], &depfile) {
        return;
    }
    run(
        {
            let mut cmd = Command::new(clang());
            cmd.arg("-O2")
                .arg("-fPIC")
                .arg("-MMD")
                .arg("-MF")
                .arg(&depfile)
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
    let depfile = dir.join(format!("{}.d", name));
    let includes = dir.join("yaci_includes.h");
    let prelude = dir.join("yaci_final_prelude.h");
    let declarations = dir.join("yaci_cmod_decls.h");
    if is_fresh(&bc, &[csrc, &includes, &prelude, &declarations], &depfile) {
        return bc;
    }
    run(
        {
            let mut cmd = Command::new(clang());
            cmd.arg("-O2")
                .arg("-flto")
                .arg("-emit-llvm")
                .arg("-MMD")
                .arg("-MF")
                .arg(&depfile)
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
    write_if_changed(
        &wrapper_rs,
        &format!(
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
    );

    let object = stmts.join(format!("{}_wrapper.o", h));
    let depfile = yaci.join(format!("yaci_s_{}.d", h));
    let includes = yaci.join("yaci_includes.h");
    let prelude = yaci.join("yaci_final_prelude.h");
    let declarations = yaci.join("yaci_cmod_decls.h");
    if is_fresh(
        &object,
        &[csrc, &wrapper_rs, &includes, &prelude, &declarations],
        &depfile,
    ) {
        return object;
    }

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
        let base = &cap.ctype;
        if cap.mutable {
            params.push(format!("{} *p{}", base, i));
            rust_params.push(format!("p{}: *mut ()", i));
            args.push(format!("&raw mut {} as *mut ()", cap.name));
        } else {
            params.push(format!("{} const *p{}", base, i));
            rust_params.push(format!("p{}: *const ()", i));
            args.push(format!("&{} as *const _ as *const ()", cap.name));
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
    write_if_changed(
        &csrc,
        &format!(
            "#include \"yaci_includes.h\"\n#include \"yaci_final_prelude.h\"\n#include \"yaci_cmod_decls.h\"\n{}{} __yaci_stmt_{}({}) {{\n{}{}{}}}\n",
            attr,
            ret_c,
            h,
            params.join(", "),
            defs,
            body,
            undefs
        ),
    );

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
