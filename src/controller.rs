use crate::history; // 历史子系统
use crate::picker;
use crate::AppContext;
use crate::{recommend_paths, RecommendOpt};

use regex::Regex;
use std::env;
use std::io::{self, Write};

/// 运行控制器：
/// - 默认模式：推荐 + 选择（交互选目录）
/// - 子命令：`cdh log --dir <path>` 追加历史日志
///
/// 退出码：
///   - 0：成功（选中 或 log 成功）
///   - 1：错误 / 用户取消 / log 失败
///   - 2：无可用候选
pub fn run(ctx: &AppContext) -> i32 {
    run_with_args(ctx, env::args().skip(1))
}

fn run_with_args(ctx: &AppContext, args: impl Iterator<Item = String>) -> i32 {
    // 0) 先看看是不是子命令：cdh log ...
    let mut args = args.peekable();

    if let Some(cmd) = args.peek() {
        if cmd == "log" {
            // 消费掉 "log" 这个单词，剩下的是 log 的参数
            args.next();
            return run_log_subcommand(ctx, args);
        }
    }

    // 1) 默认模式：构造 RecommendOpt
    let mut opt = RecommendOpt::default();

    // 1.1 用全局 Paths 覆盖历史文件路径（由 XDG 解析出来）
    opt.raw = ctx.paths.history_raw.to_string_lossy().into_owned();
    opt.uniq = ctx.paths.history_uniq.to_string_lossy().into_owned();

    // 1.2 用全局配置覆盖算法参数（ENV + 配置文件已经合并到 ctx.config 里）
    let cfg = &ctx.config;
    opt.limit = cfg.limit;
    opt.half_life = cfg.half_life;
    opt.threshold = cfg.threshold;
    opt.ignore_re = cfg.ignore_re.clone();
    opt.check_dir = cfg.check_dir;
    opt.uniq_decay = cfg.uniq_decay;
    opt.w_frecency = cfg.w_frecency;
    opt.w_uniq = cfg.w_uniq;

    // 2) 解析命令行（仅覆盖必要项；其余用 config/默认）
    // 支持：
    //  -v, --version        显示版本后退出
    //  -l, --limit <N>      返回最大条数
    //      --half-life <s>  半衰期（秒）
    //      --threshold <f>  评分阈值
    //      --ignore-re <re> 忽略路径正则
    //      --no-check-dir   不检查目录是否存在
    //      --help, -h       显示帮助
    //  其余位置参数作为 tokens 参与过滤（大小写不敏感子串）
    let mut args = args; // 复用上面的迭代器（已经消耗/判断过 log 子命令）
    while let Some(a) = args.next() {
        match a.as_str() {
            // 版本输出
            "-v" | "--version" => {
                // 版本号来自 Cargo.toml 的 [package] version
                eprintln!("cdh {}", env!("CARGO_PKG_VERSION"));
                return 0;
            }

            "-l" | "--limit" => {
                if let Some(v) = args.next() {
                    if let Ok(n) = v.parse::<usize>() {
                        opt.limit = n;
                    }
                }
            }
            "--half-life" => {
                if let Some(v) = args.next() {
                    if let Ok(secs) = v.parse::<f64>() {
                        if !secs.is_finite() || secs <= 0.0 {
                            eprintln!("cdh: --half-life 必须是大于 0 的有限数字");
                            return 1;
                        }
                        opt.half_life = secs;
                    }
                }
            }
            "--threshold" => {
                if let Some(v) = args.next() {
                    if let Ok(th) = v.parse::<f64>() {
                        opt.threshold = th;
                    }
                }
            }
            "--ignore-re" => {
                if let Some(pat) = args.next() {
                    if let Ok(rx) = Regex::new(&pat) {
                        opt.ignore_re = Some(rx);
                    }
                }
            }
            "--no-check-dir" => {
                opt.check_dir = false;
            }
            "--help" | "-h" => {
                eprintln!(
                    "用法:
  cdh [选项] [关键字...]      # 交互选择历史目录（默认模式）
  cdh log --dir <path>       # 记录一次目录访问（供 shell hook 使用）

选项:
  -v, --version          显示版本并退出
  -l, --limit <N>        返回最大条数（默认 20，可用环境变量 CDH_LIMIT 覆盖）
      --half-life <sec>  Frecency 半衰期（默认 7 天，可用 CDH_HALF_LIFE 覆盖）
      --threshold <f64>  融合分阈值（默认 0，可用 CDH_THRESHOLD 覆盖）
      --ignore-re <re>   忽略路径正则（默认取 ENV:CDH_IGNORE_RE）
      --no-check-dir     不检查目录是否存在（默认检查，可用 CDH_CHECK_DIR=false 关闭）

  其余位置参数作为过滤关键字（大小写不敏感，命中任一即可）"
                );
                return 0;
            }
            _ => {
                // 关键字过滤 token
                opt.tokens.push(a);
            }
        }
    }

    // 3) 计算推荐路径（推荐算法完全由 recommend_paths 控制）
    let paths = recommend_paths(&opt);
    if paths.is_empty() {
        return 2;
    }

    // 4) 打开 TUI 选择（非交互环境时 picker 会直接返回第一项）
    match picker::pick(&paths) {
        Ok(Some(sel)) => {
            // 与 Fish 集成友好：不换行，避免命令替换多出 \n
            print!("{sel}");
            let _ = io::stdout().flush();
            0
        }
        Ok(None) => 1, // 用户取消/超时
        Err(_e) => 1,  // 渲染异常等
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EffectiveConfig, Paths};
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_ctx(name: &str) -> (PathBuf, AppContext) {
        let uniq = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let root = std::env::temp_dir().join(format!("cdh_controller_test_{name}_{uniq}"));
        let paths = Paths {
            config_dir: root.join("config"),
            data_dir: root.join("data"),
            state_dir: root.join("state"),
            cache_dir: root.join("cache"),
            history_raw: root.join("data").join("history").join("history_raw"),
            history_uniq: root.join("data").join("history").join("history_uniq"),
        };
        fs::create_dir_all(paths.history_raw.parent().unwrap()).unwrap();
        fs::write(&paths.history_raw, format!("1\t{}\n", root.display())).unwrap();
        fs::write(&paths.history_uniq, format!("{}\n", root.display())).unwrap();
        (
            root,
            AppContext {
                paths,
                config: EffectiveConfig {
                    limit: 20,
                    half_life: 7.0 * 24.0 * 3600.0,
                    threshold: 0.0,
                    ignore_re: None,
                    check_dir: false,
                    uniq_decay: 0.85,
                    w_frecency: 0.7,
                    w_uniq: 0.3,
                },
            },
        )
    }

    #[test]
    fn half_life_zero_returns_error_instead_of_panicking() {
        let (root, ctx) = test_ctx("half_life_zero");
        let status = run_with_args(
            &ctx,
            ["--half-life", "0", "--no-check-dir"]
                .into_iter()
                .map(String::from),
        );
        assert_eq!(status, 1);
        let _ = fs::remove_dir_all(root);
    }

    #[test]
    fn half_life_negative_returns_error_instead_of_panicking() {
        let (root, ctx) = test_ctx("half_life_negative");
        let status = run_with_args(
            &ctx,
            ["--half-life", "-1", "--no-check-dir"]
                .into_iter()
                .map(String::from),
        );
        assert_eq!(status, 1);
        let _ = fs::remove_dir_all(root);
    }
}

/// 处理子命令：`cdh log --dir <path>`
///
/// 用法:
///   cdh log --dir /some/path
///   cdh log /some/path   # 简写形式, 也支持
fn run_log_subcommand(ctx: &AppContext, mut args: impl Iterator<Item = String>) -> i32 {
    let mut dir: Option<String> = None;

    while let Some(a) = args.next() {
        match a.as_str() {
            "--dir" => {
                if let Some(v) = args.next() {
                    dir = Some(v);
                } else {
                    eprintln!("cdh log: --dir 需要一个路径参数");
                    return 1;
                }
            }
            "--help" | "-h" => {
                eprintln!(
                    "用法: cdh log --dir <path>

示例:
  cdh log --dir \"$PWD\"    # 记录当前目录一次访问
  cdh log /some/path       # 简写形式"
                );
                return 0;
            }
            other => {
                // 支持简写：cdh log /path
                if dir.is_none() {
                    dir = Some(other.to_string());
                } else {
                    eprintln!("cdh log: 多余的参数: {other}");
                    return 1;
                }
            }
        }
    }

    let dir = match dir {
        Some(d) => d,
        None => {
            eprintln!("cdh log: 必须指定 --dir <path>");
            return 1;
        }
    };

    // 统一走 history 子系统的高层入口：log_visit（内部会写 raw + 更新 uniq）
    match history::log_visit(ctx, &dir) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("cdh log: 写入历史失败: {e}");
            1
        }
    }
}
