// AGENT SAFETY: 对ya_c_inline的改动是break mode，应该是大重构！现在的架构抽象有点奇怪。
use proc_macro2::{Delimiter, Spacing, TokenStream as TokenStream2, TokenTree};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use syn::spanned::Spanned;
use syn::visit::{self, Visit};

#[derive(Clone, Debug)]
struct Signature {
    captures: Vec<(String, String)>,
    ret: Option<String>,
}

#[derive(Clone)]
struct ModMacro {
    key: String,
    source: String,
    file: PathBuf,
    line: usize,
}

#[derive(Clone)]
struct StmtMacro {
    key: String,
    sig: Signature,
    source: String,
    has_return_kw: bool,
    file: PathBuf,
    line: usize,
    inferred_ret: Option<String>,
    let_types: HashMap<String, String>,
}

#[derive(Default)]
struct Collected {
    includes: Vec<String>,
    cmods: Vec<ModMacro>,
    stmts: Vec<StmtMacro>,
}

struct FileScanner {
    file: PathBuf,
    enclosing_ret: Option<String>,
    let_types: HashMap<String, String>,
    includes: Vec<String>,
    cmods: Vec<ModMacro>,
    stmts: Vec<StmtMacro>,
}

impl FileScanner {
    fn new(file: PathBuf) -> Self {
        Self {
            file,
            enclosing_ret: None,
            let_types: HashMap::new(),
            includes: Vec::new(),
            cmods: Vec::new(),
            stmts: Vec::new(),
        }
    }
}

impl<'ast> Visit<'ast> for FileScanner {
    fn visit_local(&mut self, local: &'ast syn::Local) {
        let prev = self.enclosing_ret.take();
        if let syn::Pat::Type(pat) = &local.pat {
            let ty = &pat.ty;
            let ty_str = quote::quote!(#ty).to_string();
            if let syn::Pat::Ident(id) = &*pat.pat {
                self.let_types
                    .entry(id.ident.to_string())
                    .or_insert_with(|| ty_str.clone());
            }
            self.enclosing_ret = Some(ty_str);
        }
        visit::visit_local(self, local);
        self.enclosing_ret = prev;
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        let name = m
            .path
            .segments
            .last()
            .map(|seg| seg.ident.to_string())
            .unwrap_or_default();
        if matches!(name.as_str(), "cinclude" | "cmod" | "c") {
            let key = canonical(m.tokens.clone());
            let (sig, body_tokens) = if name == "c" {
                parse_signature(m.tokens.clone())
            } else {
                (
                    Signature {
                        captures: Vec::new(),
                        ret: None,
                    },
                    m.tokens.clone(),
                )
            };
            let start_line = first_token_line(&body_tokens).unwrap_or_else(|| m.path.span().start().line);
            let mut line = start_line;
            let mut source = String::new();
            tokens_to_c(body_tokens.clone(), &mut line, &mut source);
            let source = normalize_c(&source);
            match name.as_str() {
                "cinclude" => self.includes.push(source),
                "cmod" => self.cmods.push(ModMacro {
                    key,
                    source,
                    file: self.file.clone(),
                    line: start_line,
                }),
                "c" => self.stmts.push(StmtMacro {
                    key,
                    sig,
                    has_return_kw: has_return_kw(&body_tokens),
                    source,
                    file: self.file.clone(),
                    line: start_line,
                    inferred_ret: self.enclosing_ret.clone(),
                    let_types: HashMap::new(),
                }),
                _ => {}
            }
        }
        visit::visit_macro(self, m);
    }
}

pub struct Builder {
    always_inline: bool,
    clang_args: Vec<String>,
}

impl Default for Builder {
    fn default() -> Self {
        Self::new()
    }
}

impl Builder {
    pub fn new() -> Self {
        Self {
            always_inline: false,
            clang_args: Vec::new(),
        }
    }

    /// 启用实验性 always-inline：把 c! 桩函数编译为 LLVM bitcode 并交给 LTO。
    /// 这是可选优化；关闭时 c! 仍然通过普通静态库调用正常工作。
    pub fn always_inline(mut self, enabled: bool) -> Self {
        self.always_inline = enabled;
        self
    }

    pub fn clang_arg(mut self, arg: impl Into<String>) -> Self {
        self.clang_args.push(arg.into());
        self
    }

    pub fn clang_args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.clang_args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn setup(self) {
        setup_with(self.always_inline, &self.clang_args);
    }
}

pub fn setup() {
    Builder::new().setup();
}

fn setup_with(always_inline: bool, clang_args: &[String]) {
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
    println!("cargo:rerun-if-env-changed=YA_C_INLINE_CLANG");
    println!("cargo:rerun-if-env-changed=YA_C_INLINE_LLVM_LINK");
    println!("cargo:rerun-if-env-changed=YA_C_INLINE_OPT");
    println!("cargo:rerun-if-env-changed=YA_C_INLINE_LLC");

    let src_dir = manifest.join("src");
    println!("cargo:rerun-if-changed={}", src_dir.display());
    let mut collected = Collected::default();
    collect_macros(&src_dir, &mut collected);

    let includes_path = yaci.join("yaci_includes.h");
    write_if_changed(
        &includes_path,
        &format!(
            "#pragma once\n#include <stdint.h>\n#include <stddef.h>\n#include <stdbool.h>\n{}\n",
            collected.includes.join("\n")
        ),
    );

    let cflags_path = yaci.join("cflags.stamp");
    write_if_changed(&cflags_path, &clang_args.join("\n"));

    let cmods = dedup_mods(collected.cmods);
    let mut declarations = String::from("#pragma once\n");
    for m in &cmods {
        let h = hash(&m.key);
        let csrc = mods.join(format!("{}.c", h));
        let source = format!(
            "#define YACI_CMOD_IMPL\n#include \"yaci_includes.h\"\n#line {} \"{}\"\n{}\n",
            m.line,
            escape_c_path(&m.file),
            m.source
        );
        write_if_changed(&csrc, &source);

        let lib = format!("yaci_m_{}", h);
        let depfile = yaci.join(format!("{}.d", lib));
        compile_static(&yaci, &out, &lib, &csrc, &[&csrc, &includes_path, &cflags_path], clang_args);

        let decls_file = mods.join(format!("{}.decls.h", h));
        let decls = if is_fresh(&decls_file, &out, &[&csrc, &includes_path, &cflags_path], &depfile) {
            fs::read_to_string(&decls_file).unwrap()
        } else {
            let decls = cmod_declarations(&yaci, &csrc, &source, clang_args);
            write_if_changed(&decls_file, &decls);
            decls
        };
        declarations.push_str(&decls);
        println!("cargo:rustc-link-lib=static:+whole-archive={}", lib);
    }

    let cmod_decls_path = yaci.join("yaci_cmod_decls.h");
    write_if_changed(&cmod_decls_path, &declarations);

    for stmt in dedup_stmts(collected.stmts) {
        process_stmt(
            &yaci,
            &out,
            &stmts,
            &includes_path,
            &cmod_decls_path,
            &stmt,
            always_inline,
            clang_args,
        );
    }
}

fn collect_macros(dir: &Path, out: &mut Collected) {
    if !dir.is_dir() {
        return;
    }
    println!("cargo:rerun-if-changed={}", dir.display());
    let mut paths = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().path())
        .collect::<Vec<_>>();
    paths.sort();
    for path in paths {
        if path.is_dir() {
            collect_macros(&path, out);
        } else if path.extension().and_then(|s| s.to_str()) == Some("rs") {
            println!("cargo:rerun-if-changed={}", path.display());
            let src = fs::read_to_string(&path).unwrap();
            let Ok(file) = syn::parse_file(&src) else {
                // 让 rustc 正常报告 Rust 语法错误，保留上一次生成文件
                println!(
                    "cargo:warning=ya-c-inline: skipping {} due to Rust syntax error",
                    path.display()
                );
                continue;
            };
            let mut scanner = FileScanner::new(path);
            scanner.visit_file(&file);
            out.includes.extend(scanner.includes);
            out.cmods.extend(scanner.cmods);
            for mut stmt in scanner.stmts {
                stmt.let_types = scanner.let_types.clone();
                out.stmts.push(stmt);
            }
        }
    }
}

fn process_stmt(
    yaci: &Path,
    out_dir: &Path,
    stmts: &Path,
    includes_path: &Path,
    cmod_decls_path: &Path,
    stmt: &StmtMacro,
    always_inline: bool,
    clang_args: &[String],
) {
    let h = hash(&stmt.key);
    let ret_rust = stmt.sig.ret.clone().or_else(|| stmt.inferred_ret.clone());
    if ret_rust.is_none() && stmt.has_return_kw {
        panic!(
            "ya-c-inline: {}:{}: cannot infer return type for c! with `return`; \
             write `c!{{ -> Type ... }}` or add a `let name: Type = ...` annotation",
            stmt.file.display(),
            stmt.line
        );
    }
    let ret_rust_str = ret_rust.as_deref().unwrap_or("()");
    let ret_c = ret_rust
        .as_deref()
        .map(rust_type_to_c)
        .unwrap_or_else(|| "void".to_string());

    let mut types = stmt.let_types.clone();
    for (name, ty) in &stmt.sig.captures {
        types.insert(name.clone(), ty.clone());
    }

    let meta_path = stmts.join(format!("{}.meta", h));
    write_if_changed(&meta_path, &build_stmt_meta(stmt, ret_rust_str, always_inline, clang_args));

    let rs_path = stmts.join(format!("{}.rs", h));
    let lib = format!("yaci_s_{}", h);
    let depfile = yaci.join(format!("{}.d", lib));
    let artifact = if always_inline {
        stmts.join(format!("{}_wrapper.o", h))
    } else {
        yaci.join(format!("lib{}.a", lib))
    };
    let deps = [meta_path.as_path(), includes_path, cmod_decls_path];
    if rs_path.exists()
        && is_fresh(&rs_path, out_dir, &deps, &depfile)
        && is_fresh(&artifact, out_dir, &deps, &depfile)
    {
        if always_inline {
            println!("cargo:rustc-link-arg={}", artifact.display());
        } else {
            println!("cargo:rustc-link-lib=static={}", lib);
        }
        return;
    }

    let captures = discover_captures(stmt, &ret_c, stmts, &types, clang_args);

    let mut params = Vec::new();
    let mut rust_params = Vec::new();
    let mut args = Vec::new();
    let mut defs = String::new();
    let mut undefs = String::new();
    for (i, (name, ctype, mutable)) in captures.iter().enumerate() {
        if *mutable {
            params.push(format!("{} *p{}", ctype, i));
            rust_params.push(format!("p{}: *mut ()", i));
            args.push(format!("&raw mut {} as *mut ()", name));
        } else {
            params.push(format!("{} const *p{}", ctype, i));
            rust_params.push(format!("p{}: *const ()", i));
            args.push(format!("&{} as *const _ as *const ()", name));
        }
        defs.push_str(&format!("#define {} (*p{})\n", name, i));
        undefs.push_str(&format!("#undef {}\n", name));
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
            "#include \"yaci_includes.h\"\n#include \"yaci_cmod_decls.h\"\n{}{} __yaci_stmt_{}({}) {{\n{}#line {} \"{}\"\n{}{}}}\n",
            attr,
            ret_c,
            h,
            params.join(", "),
            defs,
            stmt.line,
            escape_c_path(&stmt.file),
            stmt.source,
            undefs
        ),
    );

    let (link_attr, symbol) = if always_inline {
        let object = compile_inlined_wrapper(
            yaci,
            out_dir,
            stmts,
            &h,
            &csrc,
            includes_path,
            cmod_decls_path,
            &rust_params,
            ret_rust_str,
            clang_args,
        );
        println!("cargo:rustc-link-arg={}", object.display());
        (String::new(), format!("__yaci_rust_{}", h))
    } else {
        compile_static(yaci, out_dir, &lib, &csrc, &[&csrc, includes_path, cmod_decls_path], clang_args);
        println!("cargo:rustc-link-lib=static={}", lib);
        (
            format!("    #[link(name = \"{}\", kind = \"static\")]\n", lib),
            format!("__yaci_stmt_{}", h),
        )
    };

    write_if_changed(
        &rs_path,
        &format!(
            "{{\n{}    unsafe extern \"C\" {{\n        fn {}({}) -> {};\n    }}\n    {}({})\n}}\n",
            link_attr,
            symbol,
            rust_params.join(", "),
            ret_rust_str,
            symbol,
            args.join(", ")
        ),
    );
}

fn build_stmt_meta(stmt: &StmtMacro, ret_rust: &str, always_inline: bool, clang_args: &[String]) -> String {
    let mut meta = format!("always_inline={always_inline}\nret={ret_rust}\n");
    for arg in clang_args {
        meta.push_str(&format!("cflag:{arg}\n"));
    }
    for (name, ty) in &stmt.sig.captures {
        meta.push_str(&format!("sig:{name}={ty}\n"));
    }
    let relevant: BTreeMap<&str, &str> = stmt
        .let_types
        .iter()
        .filter(|(name, _)| contains_ident(&stmt.source, name))
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    for (name, ty) in relevant {
        meta.push_str(&format!("let:{name}={ty}\n"));
    }
    meta.push_str("---\n");
    meta.push_str(&stmt.source);
    meta
}

fn contains_ident(src: &str, ident: &str) -> bool {
    src.split(|c: char| !c.is_alphanumeric() && c != '_')
        .any(|tok| tok == ident)
}

fn discover_captures(
    stmt: &StmtMacro,
    ret_c: &str,
    stmts_dir: &Path,
    types: &HashMap<String, String>,
    clang_args: &[String],
) -> Vec<(String, String, bool)> {
    let mut declared: Vec<String> = stmt.sig.captures.iter().map(|(n, _)| n.clone()).collect();
    let probe = stmts_dir.join("__yaci_probe.c");
    let mut stdout_bytes = Vec::new();

    for _ in 0..16 {
        let decls = declared
            .iter()
            .map(|name| format!("{} {};\n", rust_type_to_c(&types[name]), name))
            .collect::<String>();
        fs::write(
            &probe,
            format!(
                "#include \"yaci_includes.h\"\n#include \"yaci_cmod_decls.h\"\n{} __yaci_probe(void) {{\n{}#line {} \"{}\"\n{}\n}}\n",
                ret_c,
                decls,
                stmt.line,
                escape_c_path(&stmt.file),
                stmt.source
            ),
        )
        .unwrap();

        let output = Command::new(clang())
            .args(clang_args)
            .args([
                "-Xclang",
                "-ast-dump=json",
                "-Xclang",
                "-ast-dump-filter=__yaci_probe",
                "-fsyntax-only",
                "-ferror-limit=0",
                "-I",
            ])
            .arg(stmts_dir.parent().unwrap())
            .arg(&probe)
            .output()
            .expect("failed to run clang for capture discovery");

        let stderr = String::from_utf8_lossy(&output.stderr);
        let fresh: Vec<String> = extract_undeclared(&stderr)
            .into_iter()
            .filter(|name| !declared.contains(name))
            .collect();

        if fresh.is_empty() {
            if !output.status.success() {
                panic!(
                    "ya-c-inline: {}:{}: C error in c!:\n{}",
                    stmt.file.display(),
                    stmt.line,
                    stderr
                );
            }
            stdout_bytes = output.stdout;
            break;
        }

        for name in &fresh {
            if !types.contains_key(name) {
                panic!(
                    "ya-c-inline: {}:{}: cannot resolve type of captured variable `{}`; \
                     write `c!{{ ({}: RustType) ... }}` or add a `let {}: RustType` annotation",
                    stmt.file.display(),
                    stmt.line,
                    name,
                    name,
                    name
                );
            }
        }
        declared.extend(fresh);
    }

    if declared.is_empty() {
        return Vec::new();
    }

    let ast: serde_json::Value = serde_json::from_slice(&stdout_bytes).unwrap_or_else(|e| {
        panic!(
            "ya-c-inline: {}:{}: invalid clang AST JSON: {}",
            stmt.file.display(),
            stmt.line,
            e
        )
    });
    let compound = ast["inner"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["kind"] == "CompoundStmt")
        .unwrap();
    let stmts = compound["inner"].as_array().unwrap();

    declared
        .into_iter()
        .zip(stmts)
        .map(|(name, decl)| {
            let id = decl["inner"][0]["id"].as_str().unwrap();
            let mutable = capture_written(compound, id, false);
            let ctype = rust_type_to_c(&types[&name]);
            (name, ctype, mutable)
        })
        .collect()
}

fn extract_undeclared(stderr: &str) -> Vec<String> {
    const NEEDLE: &str = "use of undeclared identifier '";
    let mut out = Vec::new();
    let mut rest = stderr;
    while let Some(idx) = rest.find(NEEDLE) {
        rest = &rest[idx + NEEDLE.len()..];
        let Some(end) = rest.find('\'') else {
            break;
        };
        let name = &rest[..end];
        if !name.is_empty() && !out.iter().any(|x| x == name) {
            out.push(name.to_string());
        }
        rest = &rest[end + 1..];
    }
    out
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
    node["inner"]
        .as_array()
        .into_iter()
        .flatten()
        .enumerate()
        .any(|(index, child)| capture_written(child, id, write_operand && index == 0))
}

/// 解析 `c!` 宏体开头的可选签名：`(name: RustType, ...) -> RustType`
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
    (Signature { captures, ret }, body)
}

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

fn has_return_kw(ts: &TokenStream2) -> bool {
    for tt in ts.clone() {
        match tt {
            TokenTree::Ident(id) if id == "return" => return true,
            TokenTree::Group(g) if has_return_kw(&g.stream()) => return true,
            _ => {}
        }
    }
    false
}

fn first_token_line(ts: &TokenStream2) -> Option<usize> {
    ts.clone().into_iter().next().map(|tt| tt.span().start().line)
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

fn dedup_mods(v: Vec<ModMacro>) -> Vec<ModMacro> {
    let mut seen = HashSet::new();
    v.into_iter().filter(|m| seen.insert(m.key.clone())).collect()
}

fn dedup_stmts(v: Vec<StmtMacro>) -> Vec<StmtMacro> {
    let mut seen = HashSet::new();
    v.into_iter().filter(|s| seen.insert(s.key.clone())).collect()
}

fn canonical(ts: TokenStream2) -> String {
    let mut s = String::new();
    for tt in ts {
        match tt {
            TokenTree::Ident(id) => s.push_str(&format!("i{};", id)),
            TokenTree::Literal(lit) => s.push_str(&format!("l{};", lit)),
            TokenTree::Punct(p) => {
                let spacing = if p.spacing() == Spacing::Joint { 'j' } else { 'a' };
                s.push_str(&format!("p{}{};", p.as_char(), spacing));
            }
            TokenTree::Group(g) => {
                let d = match g.delimiter() {
                    Delimiter::Brace => 'b',
                    Delimiter::Parenthesis => 'p',
                    Delimiter::Bracket => 'k',
                    Delimiter::None => 'n',
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

fn escape_c_path(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "\\\\").replace('"', "\\\"")
}

fn is_fresh(output: &Path, out_dir: &Path, inputs: &[&Path], depfile: &Path) -> bool {
    if !depfile.exists() {
        return false;
    }
    let Ok(output_time) = fs::metadata(output).and_then(|m| m.modified()) else {
        return false;
    };
    let deps = dependencies(depfile);
    let mut fresh = true;
    for path in inputs.iter().copied().chain(deps.iter().map(PathBuf::as_path)) {
        if !path.starts_with(out_dir) {
            println!("cargo:rerun-if-changed={}", path.display());
        }
        fresh &= fs::metadata(path)
            .and_then(|m| m.modified())
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
                        if p.spacing() == Spacing::Alone {
                            out.push(' ');
                        }
                    }
                }
            }
            TokenTree::Group(g) => {
                let (open, close) = match g.delimiter() {
                    Delimiter::Brace => ('{', '}'),
                    Delimiter::Parenthesis => ('(', ')'),
                    Delimiter::Bracket => ('[', ']'),
                    Delimiter::None => (' ', ' '),
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

fn tool(name: &str, env_name: &str) -> String {
    env::var(env_name).unwrap_or_else(|_| name.to_string())
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

fn cmod_declarations(dir: &Path, csrc: &Path, source: &str, clang_args: &[String]) -> String {
    let mut cmd = Command::new(clang());
    cmd.args(clang_args)
        .args(["-Xclang", "-ast-dump=json", "-fsyntax-only", "-I"])
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

fn compile_static(
    dir: &Path,
    out_dir: &Path,
    lib: &str,
    csrc: &Path,
    inputs: &[&Path],
    clang_args: &[String],
) {
    let obj = dir.join(format!("{}.o", lib));
    let archive = dir.join(format!("lib{}.a", lib));
    let depfile = dir.join(format!("{}.d", lib));
    if is_fresh(&archive, out_dir, inputs, &depfile) {
        return;
    }
    run(
        {
            let mut cmd = Command::new(clang());
            cmd.args(clang_args)
                .args(["-O2", "-fPIC", "-MMD", "-MF"])
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

fn compile_bitcode(
    dir: &Path,
    out_dir: &Path,
    name: &str,
    csrc: &Path,
    includes: &Path,
    declarations: &Path,
    clang_args: &[String],
) -> PathBuf {
    let bc = dir.join(format!("{}.bc", name));
    let depfile = dir.join(format!("{}.d", name));
    if is_fresh(&bc, out_dir, &[csrc, includes, declarations], &depfile) {
        return bc;
    }
    run(
        {
            let mut cmd = Command::new(clang());
            cmd.args(clang_args)
                .args(["-O2", "-flto", "-emit-llvm", "-MMD", "-MF"])
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

#[allow(clippy::too_many_arguments)]
fn compile_inlined_wrapper(
    yaci: &Path,
    out_dir: &Path,
    stmts: &Path,
    h: &str,
    csrc: &Path,
    includes: &Path,
    declarations: &Path,
    rust_params: &[String],
    ret_rust: &str,
    clang_args: &[String],
) -> PathBuf {
    let c_bc = compile_bitcode(
        yaci,
        out_dir,
        &format!("yaci_s_{}", h),
        csrc,
        includes,
        declarations,
        clang_args,
    );
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
    if is_fresh(&object, out_dir, &[csrc, &wrapper_rs, includes, declarations], &depfile) {
        return object;
    }

    let rust_bc = stmts.join(format!("{}_wrapper.bc", h));
    let mut rustc = Command::new("rustc");
    rustc
        .args(["--crate-type", "lib", "--emit=llvm-bc", "-C", "opt-level=2", "-A", "warnings"])
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
            cmd.args(["-filetype=obj", "-O2"])
                .arg(&inlined)
                .arg("-o")
                .arg(&object);
            cmd
        },
        "llc -filetype=obj",
    );
    object
}
