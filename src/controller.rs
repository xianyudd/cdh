use crate::{recommend_paths, RecommendOpt};
use crate::picker;

use regex::Regex;
use std::env;
use std::io::{self, Write};

/// 运行控制器：
/// 1) 解析 CLI/ENV -> RecommendOpt
/// 2) 调用智能排序 -> 路径列表
/// 3) 调用 TUI 选择器 -> 输出所选路径到 stdout（无换行）
///
/// 退出码：0 选中 / 1 取消或错误 / 2 无可用候选
pub fn run() -> i32 {
    // 1) 构造 RecommendOpt（默认从 ENV 读取：CDH_HALF_LIFE/CDH_IGNORE_RE 等）
    let mut opt = RecommendOpt::default();

    // 2) 解析命令行（仅覆盖必要项；其余用 ENV 或默认）
    // 支持：
    //  -l/--limit <N>
    //  --half-life <secs>
    //  --threshold <f64>
    //  --ignore-re <regex>
    //  --no-check-dir
    //  其余位置参数作为 tokens 参与过滤（大小写不敏感子串）
    let mut args = env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
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
                    "用法: cdh [选项] [关键字...]
  -l, --limit <N>        返回最大条数（默认取 ENV:CDH_LIMIT 或 20）
      --half-life <sec>  半衰期（秒）（默认取 ENV:CDH_HALF_LIFE 或 7*24*3600）
      --threshold <f64>  评分阈值（低于阈值的条目被过滤，默认 0 不启用）
      --ignore-re <re>   忽略路径正则（默认取 ENV:CDH_IGNORE_RE）
      --no-check-dir     不检查目录是否存在（跨机器日志时可打开）
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

    // 3) 计算推荐路径
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
        Ok(None) => 1,          // 用户取消/超时
        Err(_e) => 1,           // 渲染异常等
    }
}
