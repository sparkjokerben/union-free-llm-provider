//! 进程级配置。
//!
//! 上游池（渠道、key、条目）与运行期开关都存在 SQLite 里、后台可改；这里只放
//! 启动时必须知道、且改了要重启的东西（监听地址、数据库路径、日志目录等）。
//! 走环境变量，方便 OpenRC 的 `env` 段直接给。

use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone)]
pub struct Config {
    /// 监听地址。默认只听回环，TLS 由前面的 nginx 终结。
    pub listen: SocketAddr,
    /// SQLite 路径。
    pub db_path: PathBuf,
    /// 日志目录（按天滚动）。
    pub log_dir: PathBuf,
    /// 收到 SIGTERM 后等待在途请求结束的最长时间。
    pub drain_timeout: Duration,
    /// 请求体上限（应与 nginx 的 client_max_body_size 一致）。
    pub max_body_bytes: usize,
    /// 全局在途请求闸门；0 表示不限。
    pub max_inflight: usize,
    /// 是否以前台模式运行（OpenRC 用 supervisor 模式，需要前台）。
    pub foreground: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen: "127.0.0.1:8787".parse().expect("合法的默认监听地址"),
            db_path: PathBuf::from("/var/lib/ufp/ufp.db"),
            log_dir: PathBuf::from("/var/log/ufp"),
            drain_timeout: Duration::from_secs(600),
            max_body_bytes: 32 * 1024 * 1024,
            max_inflight: 32,
            foreground: true,
        }
    }
}

fn env_str(key: &str) -> Option<String> {
    std::env::var(key).ok().filter(|v| !v.trim().is_empty())
}

fn env_usize(key: &str) -> Option<usize> {
    env_str(key).and_then(|v| v.trim().parse().ok())
}

impl Config {
    pub fn from_env() -> Result<Self, String> {
        let mut cfg = Config::default();
        if let Some(v) = env_str("UFP_LISTEN") {
            cfg.listen = v
                .parse()
                .map_err(|e| format!("UFP_LISTEN 不是合法的监听地址（{v}）：{e}"))?;
        }
        if let Some(v) = env_str("UFP_DB") {
            cfg.db_path = PathBuf::from(v);
        }
        if let Some(v) = env_str("UFP_LOG_DIR") {
            cfg.log_dir = PathBuf::from(v);
        }
        if let Some(v) = env_usize("UFP_DRAIN_SECONDS") {
            cfg.drain_timeout = Duration::from_secs(v as u64);
        }
        if let Some(v) = env_usize("UFP_MAX_BODY_BYTES") {
            cfg.max_body_bytes = v;
        }
        if let Some(v) = env_usize("UFP_MAX_INFLIGHT") {
            cfg.max_inflight = v;
        }
        cfg.foreground = env_str("UFP_FOREGROUND").map(|v| v != "0").unwrap_or(true);
        Ok(cfg)
    }
}
