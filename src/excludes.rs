// src/excludes.rs
//! 排除清单：用户在 TUI 里主动屏蔽掉的目录子树。
//!
//! 格式：`DATA/cdh/excludes`，每行一个绝对路径，空行与 `#` 开头的行被忽略。和
//! `history_uniq` 同为行式路径文件，写入同样走「临时文件 + rename」。文件不存在
//! 等于空清单——这是常态，不是错误。
//!
//! 语义要点：
//!
//! * **一条记录排除的是该目录及其全部子目录。** 候选池从几百条历史涨到 5 万条目录
//!   树条目之后，噪音的单位是子树而不是单个目录（`~/miniforge3` 一条顶 6,000 多条），
//!   逐条排除既不现实，也会让「父目录排掉了、子目录还在」显得像坏了。
//!
//! * **清单自身保持「无相互包含」。** 插入一个已被覆盖的路径是空操作；插入一个覆盖了
//!   若干现有条目的祖先，会把被它吞掉的条目一并删掉。否则用户排掉 `~/a/b` 再排掉
//!   `~/a`，文件里会留下永远不可能再单独命中的死条目。
//!
//! 与 `CDH_IGNORE_RE` 的分工：那个是环境变量里的正则，面向「脚本/配置里预先声明」；
//! 这个是 TUI 里按一次键就能加一条的持久清单，面向「用的时候顺手清掉眼前的噪音」。
//! 两者都会作用于历史候选和发现候选。

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, BufWriter, Write};
use std::path::Path;

use crate::discover::under_prefix;

/// 一份排除清单，内部保持排序且无相互包含。
#[derive(Debug, Clone, Default)]
pub struct Excludes {
    roots: Vec<String>,
}

impl Excludes {
    /// 从文件读取；文件不存在或读不动都退化为空清单（排除清单缺失不该挡住启动）。
    pub fn load(path: &Path) -> Self {
        let Ok(file) = File::open(path) else {
            return Self::default();
        };
        let mut excludes = Self::default();
        for line in BufReader::new(file).lines().map_while(Result::ok) {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            excludes.insert(line);
        }
        excludes
    }

    #[cfg(test)]
    pub fn from_paths<I, S>(paths: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut excludes = Self::default();
        for path in paths {
            excludes.insert(path.as_ref());
        }
        excludes
    }

    pub fn is_empty(&self) -> bool {
        self.roots.is_empty()
    }

    /// `path` 是否落在某条排除记录之内（等于它，或是它的子孙）。
    pub fn contains(&self, path: &str) -> bool {
        self.roots
            .iter()
            .any(|root| under_prefix(path, root.as_str()))
    }

    /// 供 `discover::spawn` 用的剪枝集合：扫描直接不下探被排除的子树，省掉的是
    /// I/O 而不只是显示——`~/miniforge3` 那 6,000 多个目录连 `read_dir` 都不会发生。
    pub fn prune_set(&self) -> HashSet<String> {
        self.roots.iter().cloned().collect()
    }

    /// 插入一条记录，维持「无相互包含」。返回清单是否真的变了。
    pub fn insert(&mut self, dir: &str) -> bool {
        let dir = dir.trim().trim_end_matches('/');
        if dir.is_empty() {
            return false;
        }
        if self.contains(dir) {
            return false;
        }
        // 被新记录吞掉的旧记录直接删掉，避免留下不可能单独命中的死条目。
        self.roots.retain(|root| !under_prefix(root, dir));
        let owned = dir.to_string();
        let at = self.roots.partition_point(|root| root.as_str() < dir);
        self.roots.insert(at, owned);
        true
    }
}

/// 加入一条排除记录并落盘，返回更新后的清单。
///
/// 每次都重新读盘再写全量：清单是人手一条按出来的，量级在几十条，重读的成本可以
/// 忽略，换来的是别的 cdh 进程同时加的条目不会被这次写入抹掉。
pub fn add(file: &Path, dir: &str) -> io::Result<Excludes> {
    let mut excludes = Excludes::load(file);
    if excludes.insert(dir) {
        write(file, &excludes)?;
    }
    Ok(excludes)
}

fn write(file: &Path, excludes: &Excludes) -> io::Result<()> {
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }
    let tmp = file.with_extension("tmp");
    {
        let mut writer = BufWriter::new(File::create(&tmp)?);
        writeln!(writer, "# cdh excludes: one absolute path per line.")?;
        writeln!(writer, "# Each entry hides that directory and everything")?;
        writeln!(writer, "# under it, from history and from tree discovery.")?;
        for root in &excludes.roots {
            writeln!(writer, "{root}")?;
        }
        writer.flush()?;
    }
    match fs::rename(&tmp, file) {
        Ok(()) => Ok(()),
        Err(error) => {
            let _ = fs::remove_file(&tmp);
            Err(error)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn temp_file(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("cdh-excludes-{name}-{}", std::process::id()));
        let _ = fs::create_dir_all(&dir);
        dir.join("excludes")
    }

    #[test]
    fn entry_hides_the_directory_and_its_whole_subtree() {
        let excludes = Excludes::from_paths(["/home/u/miniforge3"]);
        assert!(excludes.contains("/home/u/miniforge3"));
        assert!(excludes.contains("/home/u/miniforge3/lib/python3.12"));
        // 目录边界对齐：同前缀的另一个目录名不受影响。
        assert!(!excludes.contains("/home/u/miniforge3-old"));
        assert!(!excludes.contains("/home/u/mini"));
        assert!(!excludes.contains("/home/u"));
    }

    #[test]
    fn insert_keeps_the_list_free_of_nesting() {
        let mut excludes = Excludes::default();
        assert!(excludes.insert("/a/b"));
        // 已被覆盖 -> 空操作。
        assert!(!excludes.insert("/a/b"));
        assert!(!excludes.insert("/a/b/c"));
        assert_eq!(excludes.roots, vec!["/a/b".to_string()]);
        // 祖先吞掉后代：/a/b 必须消失，否则留下死条目。
        assert!(excludes.insert("/a"));
        assert_eq!(excludes.roots, vec!["/a".to_string()]);
        // 尾斜杠归一化。
        assert!(!excludes.insert("/a/"));
    }

    #[test]
    fn load_ignores_comments_and_blank_lines() {
        let file = temp_file("load");
        fs::write(&file, "# comment\n\n/a/b\n  /c/d  \n").unwrap();
        let excludes = Excludes::load(&file);
        assert!(excludes.contains("/a/b/deep"));
        assert!(excludes.contains("/c/d"));
        assert!(!excludes.contains("/e"));
    }

    #[test]
    fn missing_file_is_an_empty_list_not_an_error() {
        let excludes = Excludes::load(Path::new("/nonexistent/cdh/excludes"));
        assert!(excludes.is_empty());
        assert!(!excludes.contains("/anything"));
    }

    #[test]
    fn add_round_trips_through_disk_and_leaves_no_temp_file() {
        let file = temp_file("add");
        let _ = fs::remove_file(&file);
        let excludes = add(&file, "/x/y").unwrap();
        assert!(excludes.contains("/x/y/z"));
        assert!(Excludes::load(&file).contains("/x/y/z"));
        // 第二条独立记录累加，不覆盖第一条。
        add(&file, "/p/q").unwrap();
        let reloaded = Excludes::load(&file);
        assert!(reloaded.contains("/x/y"));
        assert!(reloaded.contains("/p/q"));
        assert!(!file.with_extension("tmp").exists());
        let _ = fs::remove_file(&file);
    }
}
