extern crate proc_macro;

use proc_macro::TokenStream;
use proc_macro2::{Delimiter, Spacing, TokenStream as TokenStream2, TokenTree};

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

fn ident(ts: TokenStream2) -> String {
    let text = canonical(ts);
    let mut h: u64 = 0xcbf29ce484222325;
    for b in text.bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{:016x}", h)
}

fn literal_path(path: &str) -> String {
    format!("concat!(env!(\"OUT_DIR\"), \"/yaci/{}\")", path)
}

/// 收集本 crate 内所有 c! 共享的 C 头文件内容。
#[proc_macro]
pub fn cinclude(_input: TokenStream) -> TokenStream {
    TokenStream::new()
}

/// 函数外的 C 声明/定义。构建脚本会将其编译为静态库。
#[proc_macro]
pub fn cmod(input: TokenStream) -> TokenStream {
    let h = ident(input.into());
    let path = literal_path(&format!("mods/{}.h", h));
    format!("const _: &str = include_str!({path});")
        .parse()
        .expect("generated include is valid Rust")
}

/// 函数内的 C 语句。通过 YACI_CAPTURE / YACI_CAPTURE_MUT 声明捕获的 Rust 变量。
#[proc_macro]
pub fn c(input: TokenStream) -> TokenStream {
    let h = ident(input.into());
    let path = literal_path(&format!("stmts/{}.rs", h));
    format!(
        "{{\n    const _: &str = include_str!({p});\n    include!({p})\n}}",
        p = path
    )
    .parse()
    .expect("generated include is valid Rust")
}
