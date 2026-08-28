// 冲突副本的发现与处理（M3 管理功能的数据层）。
// 冲突副本是真实文件（FR-S7 命名 `name (conflict from 设备).ext`），
// 因此"处理冲突"就是纯文件操作：保留本地=删副本；采用副本=用副本覆盖原名再删副本。
// 后续同步引擎会自动把结果传播到服务端与其他设备，无需新同步逻辑。
package client

import (
	"fmt"
	"io/fs"
	"os"
	"path/filepath"
	"strings"
)

const conflictMarker = " (conflict from "

// Conflict 一条待处理的冲突。
type Conflict struct {
	Folder  string `json:"folder"`   // 所属同步文件夹名
	Rel     string `json:"rel"`      // 原始文件的相对路径
	CopyRel string `json:"copy_rel"` // 冲突副本的相对路径
	Size    int64  `json:"size"`
	MTime   int64  `json:"mtime"`
}

// ListConflicts 扫描文件夹根下的冲突副本文件。
// localPath → folderName 的映射由调用方提供（daemon 持有配置）。
func ListConflicts(root, folderName string) ([]Conflict, error) {
	var out []Conflict
	err := filepath.WalkDir(root, func(p string, d fs.DirEntry, err error) error {
		if err != nil {
			return nil
		}
		if d.IsDir() {
			name := d.Name()
			if name == ".y-sync" || name == ".git" || name == ".svn" || name == ".hg" {
				return filepath.SkipDir
			}
			return nil
		}
		base := d.Name()
		i := strings.Index(base, conflictMarker)
		if i < 0 {
			return nil
		}
		// 从文件名还原原始文件名：name (conflict from DEV)[ 2].ext → name.ext
		ext := filepath.Ext(base)
		origName := base[:i] + ext
		rel, err := filepath.Rel(root, p)
		if err != nil {
			return nil
		}
		rel = filepath.ToSlash(rel)
		dir := filepath.ToSlash(filepath.Dir(rel))
		origRel := origName
		if dir != "." {
			origRel = dir + "/" + origName
		}
		info, err := d.Info()
		if err != nil {
			return nil
		}
		out = append(out, Conflict{
			Folder:  folderName,
			Rel:     origRel,
			CopyRel: rel,
			Size:    info.Size(),
			MTime:   info.ModTime().UnixMilli(),
		})
		return nil
	})
	return out, err
}

// ResolveKeepLocal 保留原名文件：删除冲突副本（删除将同步传播）。
func ResolveKeepLocal(root string, c Conflict) error {
	if !strings.Contains(filepath.Base(c.CopyRel), conflictMarker) {
		return fmt.Errorf("%q 不是冲突副本", c.CopyRel)
	}
	return os.Remove(absJoin(root, c.CopyRel))
}

// ResolveKeepCopy 采用副本：副本内容覆盖原名文件，然后删除副本。
func ResolveKeepCopy(root string, c Conflict) error {
	if !strings.Contains(filepath.Base(c.CopyRel), conflictMarker) {
		return fmt.Errorf("%q 不是冲突副本", c.CopyRel)
	}
	copyAbs := absJoin(root, c.CopyRel)
	origAbs := absJoin(root, c.Rel)
	if _, err := os.Stat(copyAbs); err != nil {
		return err
	}
	data, err := os.ReadFile(copyAbs)
	if err != nil {
		return err
	}
	// 写临时文件再改名，与引擎的写入安全约定一致
	tmp, err := os.CreateTemp(filepath.Dir(origAbs), ".ysync-resolve-*")
	if err != nil {
		return err
	}
	tmpName := tmp.Name()
	if _, err := tmp.Write(data); err != nil {
		tmp.Close()
		os.Remove(tmpName)
		return err
	}
	if err := tmp.Close(); err != nil {
		os.Remove(tmpName)
		return err
	}
	if err := os.Rename(tmpName, origAbs); err != nil {
		os.Remove(tmpName)
		return err
	}
	return os.Remove(copyAbs)
}
