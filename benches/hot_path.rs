#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::unreachable
)]
//! postgresx 热路径基准测试：配置构造 + 校验（无网络）。
use std::hint::black_box;
use std::time::Instant;

use postgresx::{error_kind_from_sqlstate, PostgresConfig, SslMode};

fn iters() -> u32 {
    if std::env::args().any(|a| a == "--quick") {
        1_000
    } else {
        50_000
    }
}

fn build_once() -> PostgresConfig {
    PostgresConfig::builder()
        .host("127.0.0.1")
        .port(5432)
        .database("app")
        .user("app")
        .sslmode(SslMode::Disable)
        .build()
        .expect("配置构造失败")
}

fn main() {
    let n = iters();
    // 预热
    for _ in 0..n.min(50) {
        let config = build_once();
        config.validate().expect("配置校验失败");
        black_box(&config);
        black_box(error_kind_from_sqlstate("42P01"));
    }
    let start = Instant::now();
    for _ in 0..n {
        let config = build_once();
        config.validate().expect("配置校验失败");
        black_box(&config);
        // 附带覆盖 SQLSTATE 分类纯函数热路径
        black_box(error_kind_from_sqlstate("23505"));
    }
    let elapsed = start.elapsed();
    println!(
        "bench_postgresx_hot_path: iters={n} total={elapsed:?} per_iter={:?}",
        elapsed / n
    );
}
