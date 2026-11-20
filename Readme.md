# Yet Anther C Inline
这是一个允许你在Rust内插入合法C语句，函数甚至头文件的项目，个人使用，许愿式开发，不保证可用性，提示词见Design.md。

---

## 当前实现说明

C 代码交给 clang 处理；过程宏只作为标记，构建脚本负责生成胶水代码。

### 依赖

- Rust 1.82+（使用了 `&raw const` / `&raw mut`）
- `clang`、`ar`（可通过 `YA_C_INLINE_CLANG` 指定 clang 路径）
- `rust-analyzer`（仅自动捕获 Rust 变量时需要）

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

**自动捕获**：构建脚本先用 clang 找出 C 代码中未声明的标识符（即同名 Rust 变量）以及它是否被写入，再用 rust-analyzer 查询该变量在 Rust 作用域中的类型，并生成对应的捕获声明。因此常见写法不需要手写捕获：

```rust
let a: i32 = 20;
let b: i32 = 22;
let sum: i32 = unsafe {
    c! {
        return add(a, b);
    }
};
```

**类型解析失败**：自动捕获要求 rust-analyzer 能解析变量类型。如果类型来自生成的 Rust 绑定，请在调用 `setup()` 前生成绑定。不支持手写捕获类型绕过。

返回类型来自 `let` 绑定的 Rust 类型标注；无法推断时构建报错。没有 `return` 时返回 `()`。

### 实现原理

1. `setup()` 扫描 `src/**/*.rs`，用 syn 找到 `cinclude!` / `cmod!` / `c!` 的宏体，并按 token tree 计算稳定标识。
2. 宏体 token 被还原为 C 源码（保留原始行号与预处理指令换行），交给 clang：
   - `c!` 的探测阶段通过 `AnnotateAttr` 从 clang AST 中读取捕获变量名、C 类型和可变性；
3. 为每个 `c!` / `cmod!` 生成 C 包装函数和静态库，构建脚本通过 `cargo:rustc-link-lib` 链接。
4. 过程宏只展开成对生成文件的 `include!` / `include_str!` 引用，不解析任何 C 代码。

### 当前限制

- Rust 词法器会去掉宏体中的注释，C 代码中不要依赖注释。
- 返回值类型由 `let` 标注的 Rust 类型推断；支持标量、指针，以及与 C 同名且内存布局一致的结构体（用户需自行提供 C 绑定和 `#[repr(C)]` 定义）。无法推断时构建报错。
- 默认通过静态库函数调用实现；`Builder::always_inline(true)` 会把 C 桩编译为 clang bitcode，把 Rust 胶水函数编译为 LLVM bitcode，然后用 `llvm-link` 合并、`opt -passes=always-inline,globaldce` 内联、`llc` 生成 object。这样 C 代码真正进入 Rust IR；不开启时不影响正确性。

设计初衷见 [Design.md](./Design.md)。
