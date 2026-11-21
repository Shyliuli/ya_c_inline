# Yet Anther C Inline
这是一个允许你在Rust内插入合法C语句，函数甚至头文件的项目，个人使用，许愿式开发，不保证可用性，提示词见Design.md。

---

## 当前实现说明

C 代码交给 clang 处理；过程宏只作为标记，构建脚本负责生成胶水代码。

### 依赖

- Rust 1.82+（使用了 `&raw const` / `&raw mut`）
- `clang`、`ar`（可通过 `YA_C_INLINE_CLANG` 指定 clang 路径）
- always-inline 模式额外需要 `llvm-link` / `opt` / `llc`（可用 `YA_C_INLINE_LLVM_LINK` / `YA_C_INLINE_OPT` / `YA_C_INLINE_LLC` 指定）

### 使用

在用户 crate 中添加依赖和构建脚本：

```toml
[dependencies]
ya-c-inline = { git = "https://github.com/Shyliuli/ya-c-inline.git" }

[build-dependencies]
ya-c-inline-build = { git = "https://github.com/Shyliuli/ya-c-inline.git" }
```

`build.rs`：

```rust
fn main() {
    ya_c_inline_build::setup();
}
```

可选的组合式写法（用于开启实验性 always-inline）：

```rust
fn main() {
    ya_c_inline_build::Builder::new()
        .always_inline(true)
        .setup();
}
```

然后在代码中使用三个宏：

```rust
use ya_c_inline::{c, cinclude, cmod};

cinclude! {
    #include <stdio.h>
    typedef struct Point {
        int32_t x;
        int32_t y;
    } Point;
}

cmod! {
    int32_t add(int32_t a, int32_t b) { return a + b; }
    int32_t point_sum(Point p) { return p.x + p.y; }
}

fn main() {
    let a: i32 = 20;
    let b: i32 = 22;
    let sum: i32 = unsafe {
        c! {
            return add(a, b);
        }
    };
    assert_eq!(sum, 42);
}
```

### 宏说明

#### `cinclude! { ... }`

收集本 crate 内所有 `c!` / `cmod!` 共享的 C 代码（通常是 `#include` 和 `#define`）。宏体在构建阶段被原样写入生成的头文件。

#### `cmod! { ... }`

直接写 C 函数实现，不需要 `YACI_CMOD_IMPL`。构建脚本将实现编译成静态库，只把外部函数的声明提供给 `c!`。共享类型和宏放在 `cinclude!` 中。Rust 若要直接调用这些函数，请在自己的 `build.rs` 里用 bindgen 生成绑定，或手写 `extern "C"` 声明。

#### `c! { ... }`

将 C 语句插入当前函数。调用处必须写 `unsafe`。

宏体开头可以写一个**可选签名**，形式为 `(name: RustType, ...) -> RustType`；捕获列表和 `->` 返回类型各自可选，至少写一个即有显式标注：

```rust
// 显式给出捕获变量的类型（最高优先级）
let ctx: *mut box64context_t = std::ptr::null_mut();
c! {(ctx: *mut box64context_t)
    ctx->argv[0] = "box64dl";
};

// 显式给出返回类型
let sum: i32 = unsafe {
    c! { -> i32
        return add(20, 22);
    }
};

// 两者都写
let r: i32 = unsafe {
    c! {(a: i32, b: i32) -> i32
        return add(a, b);
    }
};
```

**自动捕获**：构建脚本先用 clang 找出 C 代码中未声明的标识符（即同名 Rust 变量）以及它是否被写入，再为其生成捕获声明（被写入的变量用 `&raw mut`，否则用 `&raw const`）。类型解析按以下优先级：

1. `c!` 宏体开头的显式签名 `(name: RustType, ...)` —— 最高优先级；
2. 同文件里 `let name: RustType = ...` 的类型标注；
3. 两者都没有 → 构建报错，提示补写显式签名。

常见写法依赖第 2 条即可，不需要手写签名：

```rust
let a: i32 = 20;
let b: i32 = 22;
let sum: i32 = unsafe {
    c! {
        return add(a, b);
    }
};
```

**注意**：`let name = ...` 这种无类型标注的绑定、以及 `extern static`（如 `environ`）无法被第 2 条解析，需要在签名里显式写出类型。

**返回类型**：优先级同样是 `c!` 的 `-> RustType` > `let` 绑定的类型标注 > 构建报错；没有 `return` 时返回 `()`。

显式签名里的类型是 **Rust 类型**，构建脚本会转换成对应的 C 类型；因此像 bindgen 生成的 opaque 指针也能直接写 `*mut box64context_t`。

### 实现原理

1. `setup()` 扫描 `src/**/*.rs`，用 syn 找到 `cinclude!` / `cmod!` / `c!` 的宏体，并按 token tree 计算稳定标识；`c!` 开头的可选签名在此阶段被剥离，但标识仍按完整 token 计算以与过程宏一致。
2. 宏体 token 被还原为 C 源码（保留原始行号与预处理指令换行），交给 clang：
   - `c!` 的探测阶段通过 `AnnotateAttr` 从 clang AST 中读取捕获变量名、C 类型和可变性；
   - 捕获变量类型由显式签名或 syn 解析的 `let` 标注转成 C 类型，注入为 `YACI_CAPTURE` / `YACI_CAPTURE_MUT` 声明；不依赖 rust-analyzer。
3. 为每个 `c!` / `cmod!` 生成 C 包装函数和静态库，构建脚本通过 `cargo:rustc-link-lib` 链接。
4. 过程宏只展开成对生成文件的 `include!` / `include_str!` 引用，不解析任何 C 代码。

### 当前限制

- Rust 词法器会去掉宏体中的注释，C 代码中不要依赖注释。
- 捕获/返回值类型支持标量、指针，以及与 C 同名且内存布局一致的结构体（用户需自行提供 C 绑定和 `#[repr(C)]` 定义）。`let` 无标注或类型来自生成绑定时，请用 `c!` 的显式签名；无法解析时构建报错。
- 默认通过静态库函数调用实现；`Builder::always_inline(true)` 会把 C 桩编译为 clang bitcode，把 Rust 胶水函数编译为 LLVM bitcode，然后用 `llvm-link` 合并、`opt -passes=always-inline,globaldce` 内联、`llc` 生成 object。这样 C 代码真正进入 Rust IR；不开启时不影响正确性。

设计初衷见 [Design.md](./Design.md)。
