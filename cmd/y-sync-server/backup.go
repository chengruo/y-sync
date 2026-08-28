// backup（SR5）：一致性快照——SQLite VACUUM INTO 导出元数据 + 复制全部 blob 文件。
// 快照可直接用于恢复：把导出的 y-sync.db 与 blobs/ 放回数据目录即可。
package main

import (
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"os"
	"path/filepath"
	"time"
)

func cmdBackup(args []string) error {
	fs := flag.NewFlagSet("backup", flag.ExitOnError)
	out := fs.String("out", "", "备份输出目录（必填）")
	fs.Parse(args)
	if *out == "" {
		return fmt.Errorf("usage: y-sync-server backup -out <dir>")
	}
	data := os.Getenv("YSYNC_DATA")
	if data == "" {
		data = "./y-sync-data"
	}
	dbPath := filepath.Join(data, "y-sync.db")
	if _, err := os.Stat(dbPath); err != nil {
		return fmt.Errorf("数据目录无效: %w", err)
	}
	if err := os.MkdirAll(filepath.Join(*out, "blobs"), 0o755); err != nil {
		return err
	}

	store, err := openStore()
	if err != nil {
		return err
	}
	defer store.Close()

	// 1. 元数据一致性快照（WAL 下 VACUUM INTO 生成完整独立副本）
	snapshot := filepath.Join(*out, "y-sync.db")
	if _, err := store.RawExec(fmt.Sprintf("VACUUM INTO '%s'", snapshot)); err != nil {
		return fmt.Errorf("vacuum into: %w", err)
	}

	// 2. 复制全部 blob 文件（内容不可变，直接复制安全；含正在上传的临时内容无害）
	nCopied := 0
	blobsDir := filepath.Join(data, "blobs")
	err = filepath.WalkDir(blobsDir, func(p string, d os.DirEntry, err error) error {
		if err != nil || d.IsDir() {
			return nil
		}
		rel, err := filepath.Rel(blobsDir, p)
		if err != nil {
			return nil
		}
		dst := filepath.Join(*out, "blobs", rel)
		if err := os.MkdirAll(filepath.Dir(dst), 0o755); err != nil {
			return err
		}
		return copyFile(p, dst)
	})
	if err == nil {
		count := 0
		count, err = countFiles(filepath.Join(*out, "blobs"))
		nCopied = count
	}
	if err != nil {
		return err
	}

	// 3. 清单
	manifest, _ := json.MarshalIndent(map[string]any{
		"created": time.Now().Format(time.RFC3339),
		"blobs":   nCopied,
		"note":    "恢复：将 y-sync.db 与 blobs/ 放回数据目录",
	}, "", "  ")
	if err := os.WriteFile(filepath.Join(*out, "manifest.json"), manifest, 0o644); err != nil {
		return err
	}
	fmt.Printf("backup 完成: %s（blobs=%d）\n", *out, nCopied)
	return nil
}

func copyFile(src, dst string) error {
	in, err := os.Open(src)
	if err != nil {
		return err
	}
	defer in.Close()
	out, err := os.Create(dst)
	if err != nil {
		return err
	}
	defer out.Close()
	_, err = io.Copy(out, in)
	return err
}

func countFiles(dir string) (int, error) {
	n := 0
	err := filepath.WalkDir(dir, func(_ string, d os.DirEntry, err error) error {
		if err == nil && !d.IsDir() {
			n++
		}
		return nil
	})
	return n, err
}
