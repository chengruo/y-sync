//! .syncignore：gitignore 兼容子集（FR-S8）——从 Go internal/client/ignore.go 移植，
//! 匹配语义保持一致（差分 e2e 验证）。
use regex::Regex;

pub const DEFAULT_PATTERNS: &[&str] = &[
    ".y-sync/", ".git/", ".svn/", ".hg/", //
    "*.tmp", "*~", "*.swp", ".DS_Store", "desktop.ini", "Thumbs.db", "~$*",
];

#[derive(Debug, Clone)]
struct Rule {
    re: Regex,
    negate: bool,
    dir_only: bool,
}

#[derive(Debug, Clone, Default)]
pub struct Ignore {
    rules: Vec<Rule>,
}

impl Ignore {
    /// patterns：.syncignore 内容行；默认规则在前（用户规则可覆盖默认）。
    pub fn new(patterns: &[String]) -> Self {
        let mut ig = Ignore::default();
        ig.add_rules(&DEFAULT_PATTERNS.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        ig.add_rules(patterns);
        ig
    }

    pub fn add_rules(&mut self, patterns: &[String]) {
        for raw in patterns {
            let mut line = raw.trim().to_string();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            line = line.trim_end_matches(' ').to_string();
            let mut negate = false;
            if let Some(rest) = line.strip_prefix('!') {
                negate = true;
                line = rest.to_string();
            }
            if line.is_empty() {
                continue;
            }
            let dir_only = line.ends_with('/');
            // gitignore 语义：模式含 '/'（含尾随的目录模式）则从根锚定，否则匹配任意层级
            let anchored = line.trim_end_matches('/').contains('/');
            line = line.trim_end_matches('/').trim_start_matches('/').to_string();
            if line.is_empty() {
                continue;
            }
            if let Some(re) = compile_glob(&line, anchored) {
                self.rules.push(Rule { re, negate, dir_only });
            }
        }
    }

    /// 报告相对路径（'/' 分隔）是否被忽略。
    /// 与 gitignore 一致：目录被忽略则整个子树忽略；最后一条命中的规则生效。
    pub fn matches(&self, rel_path: &str, is_dir: bool) -> bool {
        if rel_path.is_empty() {
            return false;
        }
        let mut ignored = false;
        let mut test = |p: &str, dir: bool, rules: &[Rule]| {
            for r in rules {
                if r.dir_only && !dir {
                    continue;
                }
                if r.re.is_match(p) {
                    ignored = !r.negate;
                }
            }
        };
        test(rel_path, is_dir, &self.rules);
        let parts: Vec<&str> = rel_path.split('/').collect();
        for i in 1..parts.len() {
            let parent = parts[..i].join("/");
            test(&parent, true, &self.rules);
        }
        ignored
    }
}

/// 把一条 gitignore 模式转为正则（与 Go compileGlob 一致）。
/// 未锚定（不含 '/'）的模式可命中任意层级：^(?:.*/)?seg$。
fn compile_glob(pat: &str, anchored: bool) -> Option<Regex> {
    let segs: Vec<&str> = pat.split('/').collect();
    let mut sb = String::from("^");
    if !anchored {
        sb.push_str("(?:.*/)?");
    }
    for (i, seg) in segs.iter().enumerate() {
        let last = i == segs.len() - 1;
        if *seg == "**" {
            if last {
                sb.push_str(".*");
            } else {
                // 非末段 **：吞掉零或多层完整目录段（连同其后的斜杠）
                sb.push_str("(?:[^/]+/)*");
                continue;
            }
        } else {
            sb.push_str(&glob_seg(seg));
        }
        if !last {
            sb.push('/');
        }
    }
    sb.push('$');
    Regex::new(&sb).ok()
}

fn glob_seg(seg: &str) -> String {
    let mut sb = String::new();
    for c in seg.chars() {
        match c {
            '*' => sb.push_str("[^/]*"),
            '?' => sb.push_str("[^/]"),
            '[' | ']' | '\\' | '.' | '+' | '(' | ')' | '{' | '}' | '^' | '$' | '|' => {
                sb.push('\\');
                sb.push(c);
            }
            _ => sb.push(c),
        }
    }
    sb
}

/// 读取目录下的 .syncignore 与（可选）.gitignore；无规则文件返回 None。
/// （对应 Go loadLayer，供嵌套 ignore 栈使用。）
pub fn load_layer_patterns(abs_dir: &std::path::Path, use_gitignore: bool) -> Option<Vec<String>> {
    let mut patterns: Vec<String> = Vec::new();
    if let Ok(b) = std::fs::read(abs_dir.join(".syncignore")) {
        patterns.extend(String::from_utf8_lossy(&b).lines().map(|s| s.to_string()));
    }
    if use_gitignore {
        if let Ok(b) = std::fs::read(abs_dir.join(".gitignore")) {
            patterns.extend(String::from_utf8_lossy(&b).lines().map(|s| s.to_string()));
        }
    }
    if patterns.is_empty() {
        None
    } else {
        Some(patterns)
    }
}

/// 根层规则：默认清单（FR-S17）+ .syncignore +（可选）.gitignore。
pub fn load_root_ignore(root: &std::path::Path, use_gitignore: bool) -> Ignore {
    let mut patterns: Vec<String> = DEFAULT_PATTERNS
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Ok(b) = std::fs::read(root.join(".syncignore")) {
        patterns.extend(String::from_utf8_lossy(&b).lines().map(|s| s.to_string()));
    }
    if use_gitignore {
        if let Ok(b) = std::fs::read(root.join(".gitignore")) {
            patterns.extend(String::from_utf8_lossy(&b).lines().map(|s| s.to_string()));
        }
    }
    Ignore::new(&[])
        .with_extra(patterns)
}

impl Ignore {
    pub fn with_extra(mut self, patterns: Vec<String>) -> Self {
        self.add_rules(&patterns);
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ig(patterns: &[&str]) -> Ignore {
        Ignore::new(&patterns.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn gitignore_semantics() {
        let ig = ig(&["*.log", "/rootonly/", "build", "!keep.log", "docs/**/*.md"]);
        let cases: Vec<(&str, bool, bool)> = vec![
            ("debug.log", false, true),
            ("sub/debug.log", false, true),
            ("keep.log", false, false),
            ("sub/keep.log", false, false),
            ("rootonly/x.txt", true, true),
            ("rootonly/x.txt", false, true),
            ("a/rootonly/x.txt", false, false), // 锚定：不匹配子目录同名目录
            ("src/build", true, true),
            ("src/build/x.go", false, true),
            ("builder", false, false),
            ("docs/a/b.md", false, true),
            ("docs/x.md", false, true),
            ("other/x.md", false, false),
            ("a.txt", false, false),
        ];
        for (path, is_dir, want) in cases {
            assert_eq!(ig.matches(path, is_dir), want, "path={path} dir={is_dir}");
        }
    }
}
