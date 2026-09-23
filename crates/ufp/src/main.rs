//! ufp —— 统一 LLM 中转网关。
//!
//! 下游：Anthropic Messages 协议、单个模型 id、多个下游 key（客户端是 Claude Code）。
//! 上游：OpenAI Chat / OpenAI Responses / Gemini 原生 / Anthropic 原生的免费额度池。
//!
//! 子命令：
//! - `ufp serve`（默认）启动网关
//! - `ufp set-admin-password [密码]` 设置后台密码（不传则从 stdin 读一行）

use ufp::{api, config, health, router, store};

use std::path::Path;
use std::process::ExitCode;
use std::time::Duration;

use socket2::{Domain, Protocol, Socket, Type};

#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("serve");
    match cmd {
        "serve" => match run_serve() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("ufp 启动失败：{e}");
                ExitCode::FAILURE
            }
        },
        "set-deploy-token" => {
            match set_deploy_token() {
                Ok(token) => {
                    println!("部署令牌已写入（只显示这一次，复制到仓库 Secret DEPLOY_TOKEN）：\n\n{token}\n");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("设置部署令牌失败：{e}");
                    ExitCode::FAILURE
                }
            }
        }
        "set-admin-password" => match set_admin_password(args.get(2).map(String::as_str)) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("设置后台密码失败：{e}");
                ExitCode::FAILURE
            }
        },
        "version" | "-v" | "--version" => {
            println!("ufp {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        "help" | "-h" | "--help" => {
            println!(
                "ufp {}\n\n用法：\n  ufp serve                      启动网关（环境变量见 deploy/openrc/ufp）\n  ufp set-admin-password [密码]   设置后台登录密码，不传则从 stdin 读一行\n  ufp set-deploy-token           生成新的部署令牌（CI 用）\n  ufp version",
                env!("CARGO_PKG_VERSION")
            );
            ExitCode::SUCCESS
        }
        other => {
            eprintln!("未知子命令：{other}（试试 ufp help）");
            ExitCode::from(2)
        }
    }
}

fn run_serve() -> Result<(), Box<dyn std::error::Error>> {
    let cfg = config::Config::from_env().map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let _guard = init_logging(&cfg.log_dir, cfg.foreground);
    tracing::info!(
        version = env!("CARGO_PKG_VERSION"),
        listen = %cfg.listen,
        db = %cfg.db_path.display(),
        "ufp 启动中"
    );

    let workers = std::env::var("UFP_WORKERS")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get().clamp(1, 4))
                .unwrap_or(2)
        });
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(workers)
        .thread_name("ufp-worker")
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let db = store::Db::open(&cfg.db_path)?;
        let pool = db.read(store::Pool::load).await?;
        if pool.entries.is_empty() {
            tracing::warn!("池里没有任何可用条目，先去后台添加渠道 / key / 条目");
        }
        // 冷却与会话粘性从库里恢复：两者都跨重启生效。
        let cooldowns = std::sync::Arc::new(health::Cooldowns::new());
        let sessions = std::sync::Arc::new(router::Sessions::new());
        {
            let (c, s) = (
                std::sync::Arc::clone(&cooldowns),
                std::sync::Arc::clone(&sessions),
            );
            let _ = db
                .read(move |conn| {
                    let n = c.load(conn)?;
                    let m = s.load(conn)?;
                    tracing::info!(cooldowns = n, sessions = m, "已恢复冷却与会话粘性");
                    Ok(())
                })
                .await;
        }
        let snapshot = store::Snapshot::new(pool);
        let state = api::AppState::new(cfg.clone(), db, snapshot, cooldowns, sessions);
        spawn_maintenance(std::sync::Arc::clone(&state));
        let app = api::router(state.clone());

        let listener = bind_listener(cfg.listen)?;
        tracing::info!(addr = %cfg.listen, "开始监听");

        // 排空：收到信号后不再接新连接，等在途请求结束；超过 drain_timeout 强制退出。
        let (drain_tx, drain_rx) = tokio::sync::oneshot::channel::<()>();
        tokio::spawn(async move {
            wait_for_shutdown_signal().await;
            tracing::info!("收到退出信号，停止接受新连接，等待在途请求结束");
            let _ = drain_tx.send(());
            tokio::time::sleep(Duration::from_secs(0)).await;
        });
        let hard = tokio::spawn({
            let timeout = cfg.drain_timeout;
            async move {
                wait_for_shutdown_signal().await;
                tokio::time::sleep(timeout).await;
                tracing::warn!("排空超时，强制退出");
                std::process::exit(0);
            }
        });

        axum::serve(listener, app)
            .with_graceful_shutdown(async move {
                let _ = drain_rx.await;
            })
            .await?;
        hard.abort();
        tracing::info!("已退出");
        Ok::<(), Box<dyn std::error::Error>>(())
    })?;
    Ok(())
}

/// 后台维护任务。
///
/// 每小时清理一次过期状态（熔断器里长期不活跃的条目、过期冷却、过期会话映射）；
/// 每天做一次明细汇总、过期数据清理与 SQLite 备份（后续随存储模块补齐）。
fn spawn_maintenance(state: std::sync::Arc<api::AppState>) {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(3600));
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        tick.tick().await; // 第一次立刻返回，跳过
        let mut rounds: u64 = 0;
        // 启动时先跑一次每日维护（重启后也能补上汇总与备份）
        let mut run_daily_now = true;
        loop {
            tick.tick().await;
            let now = chrono::Utc::now().timestamp_millis();
            state.breakers.prune(24 * 3600 * 1000);
            let cooldowns = state.cooldowns.prune(now);
            let settings = state.pool.load().settings.clone();
            let sessions = state.sessions.prune(&state.db, settings.session_ttl_days);
            tracing::info!(cooldowns, sessions, "维护：已清理过期的冷却与会话映射");

            rounds += 1;
            if rounds % 24 == 0 || run_daily_now {
                run_daily_now = false;
                let cutoff = now - settings.detail_retention_days as i64 * 86_400_000;
                match store::maintenance::run_daily(
                    &state.db,
                    cutoff,
                    settings.session_ttl_days,
                    &state.backup_dir,
                )
                .await
                {
                    Ok(r) => tracing::info!(
                        rolled_up = r.rolled_up,
                        pruned_details = r.pruned_details,
                        pruned_attempts = r.pruned_attempts,
                        backup = ?r.backup,
                        "每日维护：明细已汇总，过期数据已清理"
                    ),
                    Err(e) => tracing::warn!(error = %e, "每日维护失败"),
                }
                let dropped = state.db.dropped();
                if dropped > 0 {
                    state.alerter.notify(
                        &settings.alerts,
                        "db_write_dropped",
                        "ufp：写库开始丢数据",
                        &format!(
                            "已经有 {dropped} 条用量记录因为写队列拥塞被丢弃。\
                             通常是磁盘满了或写入异常，请检查服务器。"
                        ),
                    );
                }
            }
        }
    });
}

fn bind_listener(
    addr: std::net::SocketAddr,
) -> Result<tokio::net::TcpListener, Box<dyn std::error::Error>> {
    let domain = if addr.is_ipv4() {
        Domain::IPV4
    } else {
        Domain::IPV6
    };
    let sock = Socket::new(domain, Type::STREAM, Some(Protocol::TCP))?;
    sock.set_reuse_address(true)?;
    // SO_REUSEPORT：升级时新旧进程可以同时监听同一端口，交接期间不断流。
    let _ = sock.set_reuse_port(true);
    sock.set_nonblocking(true)?;
    sock.bind(&addr.into())?;
    sock.listen(1024)?;
    let std_listener: std::net::TcpListener = sock.into();
    Ok(tokio::net::TcpListener::from_std(std_listener)?)
}

async fn wait_for_shutdown_signal() {
    use tokio::signal::unix::{signal, SignalKind};
    let mut term = match signal(SignalKind::terminate()) {
        Ok(s) => s,
        Err(_) => {
            let _ = tokio::signal::ctrl_c().await;
            return;
        }
    };
    let mut int = match signal(SignalKind::interrupt()) {
        Ok(s) => s,
        Err(_) => {
            let _ = term.recv().await;
            return;
        }
    };
    tokio::select! {
        _ = term.recv() => {}
        _ = int.recv() => {}
    }
}

fn init_logging(
    log_dir: &Path,
    foreground: bool,
) -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::{fmt, EnvFilter};

    let filter = EnvFilter::try_from_env("UFP_LOG").unwrap_or_else(|_| EnvFilter::new("info"));
    let mut guard = None;

    let file_layer = match std::fs::create_dir_all(log_dir) {
        Ok(()) => {
            let appender = tracing_appender::rolling::daily(log_dir, "ufp.log");
            let (writer, g) = tracing_appender::non_blocking(appender);
            guard = Some(g);
            Some(fmt::layer().with_writer(writer).with_ansi(false))
        }
        Err(e) => {
            eprintln!("无法创建日志目录 {}：{e}", log_dir.display());
            None
        }
    };

    let stderr_layer = if foreground {
        Some(fmt::layer().with_writer(std::io::stderr))
    } else {
        None
    };

    let subscriber = tracing_subscriber::registry()
        .with(filter)
        .with(file_layer)
        .with(stderr_layer);
    if tracing::subscriber::set_global_default(subscriber).is_err() {
        eprintln!("日志订阅器已被设置过，忽略重复初始化");
    }
    guard
}

/// 生成并写入部署令牌（CI 往 /admin/api/deploy 推新版本时用）。
fn set_deploy_token() -> Result<String, Box<dyn std::error::Error>> {
    use rand::Rng;
    let cfg = config::Config::from_env().map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let token: String = rand::thread_rng()
        .sample_iter(&rand::distributions::Alphanumeric)
        .take(48)
        .map(char::from)
        .collect();
    let db = store::Db::open(&cfg.db_path)?;
    let token_for_db = token.clone();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            db.admin(move |conn| {
                let mut settings = store::load_settings(conn)?;
                settings.deploy_token = token_for_db;
                store::save_settings(conn, &settings)?;
                Ok(())
            })
            .await
        })?;
    Ok(token)
}

fn set_admin_password(arg: Option<&str>) -> Result<(), Box<dyn std::error::Error>> {
    use argon2::password_hash::{rand_core::OsRng, PasswordHasher, SaltString};
    use argon2::Argon2;

    let cfg = config::Config::from_env().map_err(|e| -> Box<dyn std::error::Error> { e.into() })?;
    let password = match arg {
        Some(p) if !p.is_empty() => p.to_string(),
        _ => {
            eprintln!("请在后端输入中粘贴后台密码后回车（交互式输入会回显，介意的话用管道传入）：");
            let mut line = String::new();
            std::io::stdin().read_line(&mut line)?;
            line.trim().to_string()
        }
    };
    if password.len() < 8 {
        return Err("密码至少 8 位".into());
    }
    let salt = SaltString::generate(&mut OsRng);
    let hash = Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| format!("哈希失败：{e}"))?
        .to_string();

    let db = store::Db::open(&cfg.db_path)?;
    let hash_clone = hash.clone();
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async move {
            db.admin(move |conn| {
                conn.execute(
                    "INSERT INTO admin (id, password_hash, updated_ms) VALUES (1, ?1, ?2)
                     ON CONFLICT(id) DO UPDATE SET password_hash = ?1, updated_ms = ?2",
                    rusqlite::params![hash_clone, chrono::Utc::now().timestamp_millis()],
                )?;
                Ok(())
            })
            .await
        })?;
    println!("后台密码已更新（存于 {}）", cfg.db_path.display());
    Ok(())
}
