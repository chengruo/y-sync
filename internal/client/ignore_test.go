package client

import "testing"

func TestIgnoreMatch(t *testing.T) {
	ig := NewIgnore([]string{
		"*.log",      // 无斜杠：匹配任意层级
		"/rootonly/", // 锚定目录
		"build",      // 目录名，任意层级
		"!keep.log",  // 取反
		"docs/**/*.md",
	})
	cases := []struct {
		path  string
		isDir bool
		want  bool
	}{
		{"debug.log", false, true},
		{"sub/debug.log", false, true},
		{"keep.log", false, false},         // !keep.log 覆盖
		{"sub/keep.log", false, false},     // 取反在任意层
		{"rootonly/x.txt", true, true},     // 锚定目录（自身）
		{"rootonly/x.txt", false, true},    // 父目录被忽略 → 忽略
		{"a/rootonly/x.txt", false, false}, // 锚定：不匹配子目录里的同名目录（gitignore 语义）
		{"src/build", true, true},          // 任意层 build 目录
		{"src/build/x.go", false, true},    // 父目录被忽略
		{"builder", false, false},          // 前缀不误伤
		{"docs/a/b.md", false, true},       // ** 跨层
		{"docs/x.md", false, true},         // ** 可匹配零层
		{"other/x.md", false, false},
		{"a.txt", false, false},
	}
	for _, c := range cases {
		if got := ig.Match(c.path, c.isDir); got != c.want {
			t.Errorf("Match(%q, dir=%v) = %v, want %v", c.path, c.isDir, got, c.want)
		}
	}
}
