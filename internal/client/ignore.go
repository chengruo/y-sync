// .syncignore：gitignore 兼容子集（FR-S8）。
// 支持：注释/空行、`!` 取反（后匹配优先）、`**` 跨层、`/` 锚定、结尾 `/` 仅目录。
// 另有默认忽略清单（FR-S17）：.y-sync/ .git/ .svn/ .hg/ 及常见临时文件。
package client

import (
	"regexp"
	"strings"
)

type ignoreRule struct {
	re      *regexp.Regexp
	negate  bool
	dirOnly bool
}

type Ignore struct {
	rules []ignoreRule
}

var defaultPatterns = []string{
	".y-sync/", ".git/", ".svn/", ".hg/",
	"*.tmp", "*~", "*.swp", ".DS_Store", "desktop.ini", "Thumbs.db", "~$*",
}

// NewIgnore 从模式列表构建；patterns 前面是 .syncignore 的，后面是默认的。
func NewIgnore(patterns []string) *Ignore {
	ig := &Ignore{}
	// 默认规则在前，.syncignore 在后（用户规则可覆盖默认）
	ig.addRules(defaultPatterns)
	ig.addRules(patterns)
	return ig
}

func (ig *Ignore) addRules(patterns []string) {
	for _, raw := range patterns {
		line := strings.TrimSpace(raw)
		if line == "" || strings.HasPrefix(line, "#") {
			continue
		}
		line = strings.TrimSuffix(line, " ") // 尾随空格除非转义——简化处理：直接去除
		negate := false
		if strings.HasPrefix(line, "!") {
			negate = true
			line = line[1:]
		}
		if line == "" {
			continue
		}
		dirOnly := strings.HasSuffix(line, "/")
		// gitignore 语义：模式含 '/'（含尾随的目录模式）则从根锚定，否则匹配任意层级
		anchored := strings.Contains(strings.TrimSuffix(line, "/"), "/")
		line = strings.TrimSuffix(line, "/")
		line = strings.TrimPrefix(line, "/") // 锚定标志已记录，斜杠本身不参与匹配
		if line == "" {
			continue
		}
		ig.rules = append(ig.rules, ignoreRule{
			re: compileGlob(line, anchored), negate: negate, dirOnly: dirOnly,
		})
	}
}

// compileGlob 把一条 gitignore 模式转为正则。
// 未锚定（不含 '/'）的模式可命中任意层级：^(?:.*/)?seg$。
func compileGlob(pat string, anchored bool) *regexp.Regexp {
	segs := strings.Split(pat, "/")
	var sb strings.Builder
	sb.WriteString("^")
	if !anchored {
		sb.WriteString("(?:.*/)?")
	}
	for i, seg := range segs {
		last := i == len(segs)-1
		if seg == "**" {
			if last {
				sb.WriteString(".*")
			} else {
				// 非末段 **：吞掉零或多层完整目录段（连同其后的斜杠）
				sb.WriteString("(?:[^/]+/)*")
				continue
			}
		} else {
			sb.WriteString(globSeg(seg))
		}
		if !last {
			sb.WriteString("/")
		}
	}
	sb.WriteString("$")
	re, err := regexp.Compile(sb.String())
	if err != nil {
		// 理论不可达：globSeg 产出的字符类是安全的
		re = regexp.MustCompile(`^\z.`)
	}
	return re
}

func globSeg(seg string) string {
	var sb strings.Builder
	for i := 0; i < len(seg); i++ {
		c := seg[i]
		switch c {
		case '*':
			sb.WriteString("[^/]*")
		case '?':
			sb.WriteString("[^/]")
		case '[', ']', '\\', '.', '+', '(', ')', '{', '}', '^', '$', '|':
			sb.WriteByte('\\')
			sb.WriteByte(c)
		default:
			sb.WriteByte(c)
		}
	}
	return sb.String()
}

// Match 报告相对路径（'/' 分隔）是否被忽略。
// 语义与 gitignore 一致：对目录路径匹配任一父层规则（目录被忽略则整个子树忽略）；
// 最后一条命中的规则生效（支持 `!` 取反）。
func (ig *Ignore) Match(relPath string, isDir bool) bool {
	if relPath == "" {
		return false
	}
	ignored := false
	test := func(p string, dir bool) {
		for _, r := range ig.rules {
			if r.dirOnly && !dir {
				continue
			}
			if r.re.MatchString(p) {
				ignored = !r.negate
			}
		}
	}
	// 路径本身
	test(relPath, isDir)
	// 各级父目录（目录被忽略 → 子树全忽略；取反规则可再次覆盖）
	parts := strings.Split(relPath, "/")
	for i := 1; i < len(parts); i++ {
		test(strings.Join(parts[:i], "/"), true)
	}
	return ignored
}
