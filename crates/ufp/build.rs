//! 把发布版本号注入二进制。
//!
//! 版本来源优先级：
//! 1. `crates/ufp/version.txt`（发布流水线在构建前写入 git tag，例如 `0.1.2`）；
//! 2. 环境变量 `UFP_BUILD_VERSION`；
//! 3. 回退到 Cargo.toml 的版本（本地开发用）。
//!
//! 为什么要这么绕：健康检查 `/healthz` 里的 version 是部署流水线判断
//! 「新版本是否真的上线」的依据，所以它必须跟着 git tag 走，而不是跟着
//! Cargo.toml（否则每次发版都得记得改 Cargo.toml，忘了就会让部署校验失配）。

fn main() {
    println!("cargo:rerun-if-changed=version.txt");
    println!("cargo:rerun-if-env-changed=UFP_BUILD_VERSION");

    let version = std::fs::read_to_string("version.txt")
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '+'))
        .or_else(|| std::env::var("UFP_BUILD_VERSION").ok())
        .unwrap_or_else(|| std::env::var("CARGO_PKG_VERSION").unwrap_or_else(|_| "0.0.0".into()));
    println!("cargo:rustc-env=UFP_VERSION={version}");
}
