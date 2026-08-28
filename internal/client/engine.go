// 同步引擎（§4.3）：对每个同步文件夹独立执行 reconcile 双向对账。
// 上行：本地变更 → 两阶段提交（先传内容再批量元数据操作）；
// 下行：凭 cursor 增量拉取变更日志并应用；冲突保留双方（FR-S7）。
package client

import (
	"crypto/sha256"
	"encoding/hex"
	"fmt"
	"io"
	"log/slog"
	"os"
	"path/filepath"
	"sort"
	"strings"
	"time"

	"ysync/internal/protocol"
)

type Engine struct {
	Cfg *Config
	API *API
	Log *slog.Logger
}

// diskInfo 本地扫描得到的一条记录
type diskInfo struct {
	Size  int64
	MTime int64 // unix 毫秒
	IsDir bool
}

type moveRec struct {
	old, new, hash string
}

// ---------- 本地扫描 ----------

// ignLayer 一层忽略规则（.syncignore/.gitignore 所在目录 + 规则集）。
type ignLayer struct {
	base string // 相对路径，"" 为根；规则匹配以 base 为相对根
	ig   *Ignore
}

// ignStack 分层规则栈：深层文件的 .syncignore 覆盖浅层（gitignore 语义）。
type ignStack []ignLayer

func (st ignStack) match(rel string, isDir bool) bool {
	ignored := false
	for _, l := range st {
		if l.base != "" {
			if rel == l.base || !strings.HasPrefix(rel, l.base+"/") {
				continue // 该层规则不适用于其所在目录自身
			}
			rel = strings.TrimPrefix(rel, l.base+"/")
		}
		if l.ig.Match(rel, isDir) {
			ignored = true // 后压栈（更深层）的判定覆盖浅层
		}
	}
	return ignored
}

// walkLocal 递归扫描：逐目录压栈 .syncignore（及可选 .gitignore），
// 排除 excluded 前缀子树（FR-S9）与被忽略子树。
func walkLocal(root string, rootIg *Ignore, useGitignore bool, excluded func(rel string) bool) (map[string]diskInfo, error) {
	out := map[string]diskInfo{}
	var walk func(absDir, rel string, st ignStack) error
	walk = func(absDir, rel string, st ignStack) error {
		ents, err := os.ReadDir(absDir)
		if err != nil {
			return err
		}
		// 本目录的规则文件 → 新层
		layer := loadLayer(absDir, rel, useGitignore)
		if layer != nil {
			st = append(st, *layer)
		}
		for _, e := range ents {
			name := e.Name()
			childRel := name
			if rel != "" {
				childRel = rel + "/" + name
			}
			isDir := e.IsDir()
			if excluded != nil && excluded(childRel) {
				continue // 选择性同步排除（FR-S9）
			}
			if st.match(childRel, isDir) {
				continue // 被忽略：目录不递归（FR-S8）
			}
			info, err := e.Info()
			if err != nil {
				continue
			}
			if isDir {
				out[childRel] = diskInfo{IsDir: true}
				if err := walk(filepath.Join(absDir, name), childRel, st); err != nil {
					return err
				}
				continue
			}
			if !e.Type().IsRegular() {
				continue // 符号链接等非常规文件不同步（§4.4）
			}
			out[childRel] = diskInfo{Size: info.Size(), MTime: info.ModTime().UnixMilli()}
		}
		return nil
	}
	return out, walk(root, "", ignStack{{base: "", ig: rootIg}})
}

// loadLayer 读取目录下的 .syncignore 与（可选）.gitignore；无规则文件返回 nil。
func loadLayer(absDir, rel string, useGitignore bool) *ignLayer {
	var patterns []string
	if b, err := os.ReadFile(filepath.Join(absDir, ".syncignore")); err == nil {
		patterns = append(patterns, strings.Split(string(b), "\n")...)
	}
	if useGitignore {
		if b, err := os.ReadFile(filepath.Join(absDir, ".gitignore")); err == nil {
			patterns = append(patterns, strings.Split(string(b), "\n")...)
		}
	}
	if len(patterns) == 0 {
		return nil
	}
	return &ignLayer{base: rel, ig: NewIgnore(patterns)}
}

func hashLocalFile(root, rel string) (string, int64, error) {
	return hashAndSize(absJoin(root, rel))
}

// hashAndSize 流式计算 SHA-256 与大小（一次遍历）
func hashAndSize(path string) (string, int64, error) {
	f, err := os.Open(path)
	if err != nil {
		return "", 0, err
	}
	defer f.Close()
	h := sha256.New()
	n, err := io.Copy(h, f)
	if err != nil {
		return "", 0, err
	}
	return hex.EncodeToString(h.Sum(nil)), n, nil
}

// ---------- 单文件夹同步 ----------

type syncStats struct {
	Uploaded, Downloaded, Moved, Deleted, Conflicts int
}

// SyncFolder 同步一个文件夹，返回统计。
func (e *Engine) SyncFolder(f *Folder) (syncStats, error) {
	var stats syncStats
	root := f.LocalPath
	abs, err := filepath.Abs(root)
	if err != nil {
		return stats, err
	}
	if err := os.MkdirAll(abs, 0o755); err != nil {
		return stats, err
	}
	root = abs

	// 崩溃恢复（M2）：上次 ops 提交后状态未持久化 → 丢弃本地状态库，
	// cursor 归零触发全量对账（内容已按哈希去重，代价仅为重扫描）
	if pendingMarkerExists(root) {
		e.Log.Warn("检测到未完成的元数据提交，重建本地状态", "folder", f.Name)
		resetStateDB(root)
		clearPendingMarker(root)
		f.Cursor = 0
	}

	st, err := OpenState(root)
	if err != nil {
		return stats, fmt.Errorf("open state: %w", err)
	}
	defer st.Close()

	// 限速与传输策略（FR-S11/S12）
	e.Cfg.Defaults()
	e.API.SetLimits(e.Cfg.UploadLimitKBs, e.Cfg.DownloadLimitKBs)

	ig := LoadIgnore(root, f.UseGitignore)
	excludedFn := func(rel string) bool {
		for _, ex := range f.Excludes {
			if rel == ex || strings.HasPrefix(rel, ex+"/") {
				return true
			}
		}
		return false
	}

	// 1. 解析服务端子树根
	rootID := f.RootNodeID
	if rootID == 0 {
		rootID, err = e.resolveRoot(f)
		if err != nil {
			return stats, err
		}
		f.RootNodeID = rootID
		SaveConfig(e.Cfg)
	}

	// 2. 基线（上次同步完成时的本地状态 = 服务端视图）
	baseline, err := st.All()
	if err != nil {
		return stats, err
	}
	baselineByNode := map[int64]string{} // node_id → rel
	for rel, r := range baseline {
		baselineByNode[r.NodeID] = rel
	}

	// 3. 服务端当前视图
	prefix := f.Name + "/"
	serverNow := map[string]protocol.NodeInfo{} // rel → 节点
	changedSet := map[string]bool{}
	var newCursor int64

	if f.Cursor == 0 {
		// 初次同步：全量拉取该子树
		nodes, err := e.API.Nodes()
		if err != nil {
			return stats, err
		}
		for _, n := range nodes {
			if n.Path != f.Name && !strings.HasPrefix(n.Path, prefix) {
				continue
			}
			rel := strings.TrimPrefix(n.Path, prefix)
			if rel == "" {
				continue
			}
			serverNow[rel] = n
			changedSet[rel] = true
		}
		newCursor, err = e.API.Head()
		if err != nil {
			return stats, err
		}
	} else {
		// 增量：应用变更日志到基线
		for rel, r := range baseline {
			serverNow[rel] = protocol.NodeInfo{ID: r.NodeID, ContentHash: r.Hash, Size: r.Size,
				MTime: r.MTime, Type: r.Type, Name: filepath.Base(rel), Path: f.Name + "/" + rel}
		}
		var head int64
		for {
			resp, err := e.API.Changes(f.Cursor, 1000, rootID)
			if err != nil {
				return stats, err
			}
			head = resp.Cursor
			for _, c := range resp.Changes {
				if c.DeviceID == e.Cfg.DeviceID {
					continue // 自己设备的变更已反映在本地状态中（避免伪冲突）
				}
				if c.Path != f.Name && !strings.HasPrefix(c.Path, prefix) {
					continue
				}
				rel := strings.TrimPrefix(c.Path, prefix)
				if rel == "" {
					continue
				}
				if c.Op == protocol.OpUnlink {
					if oldRel, ok := baselineByNode[c.NodeID]; ok {
						delete(serverNow, oldRel)
						changedSet[oldRel] = true
						delete(baselineByNode, c.NodeID)
					} else {
						delete(serverNow, rel)
						changedSet[rel] = true
					}
					continue
				}
				// put/mkdir/move：节点现在位于 rel
				if oldRel, ok := baselineByNode[c.NodeID]; ok && oldRel != rel {
					delete(serverNow, oldRel)
					changedSet[oldRel] = true
				}
				baselineByNode[c.NodeID] = rel
				serverNow[rel] = protocol.NodeInfo{ID: c.NodeID, ParentID: c.ParentID, Name: c.Name,
					Type: c.Type, Size: c.Size, MTime: c.MTime, ContentHash: c.ContentHash, Path: c.Path}
				changedSet[rel] = true
			}
			if len(resp.Changes) < 1000 {
				break
			}
			f.Cursor = resp.Changes[len(resp.Changes)-1].Cursor
		}
		newCursor = head
	}

	// 4. 本地扫描
	scan, err := walkLocal(root, ig, f.UseGitignore, excludedFn)
	if err != nil {
		return stats, err
	}

	// 5. 计算本地变更集
	modified := map[string]string{} // rel → 新内容哈希（内容有变）
	mtimeOnly := map[string]bool{}  // rel → 仅 mtime 变化
	newFiles := map[string]string{} // rel → 哈希（本地新增文件）
	localDirs := map[string]bool{}  // 本地新增目录
	del := map[string]bool{}        // 本地删除

	for rel, d := range scan {
		base, had := baseline[rel]
		if d.IsDir {
			if !had || base.Type != protocol.TypeDir {
				localDirs[rel] = true
			}
			continue
		}
		if had && base.Type == protocol.TypeFile {
			if d.Size == base.Size && d.MTime == base.MTime {
				continue // 未变
			}
			h, _, err := hashLocalFile(root, rel)
			if err != nil {
				return stats, err
			}
			if h == base.Hash {
				mtimeOnly[rel] = true
			} else {
				modified[rel] = h
			}
		} else {
			h, _, err := hashLocalFile(root, rel)
			if err != nil {
				return stats, err
			}
			newFiles[rel] = h
		}
	}
	for rel, base := range baseline {
		if _, ok := scan[rel]; !ok {
			if base.Type == protocol.TypeDir && hasDescendant(scan, rel) {
				continue // 目录内还有文件：按文件级删除处理
			}
			del[rel] = true
		}
	}

	// 6. 本地重命名检测：同哈希同大小的 删除+新增 对 → move（FR-S6）
	moves := detectMoves(del, newFiles, baseline, scan)
	moveSrc := map[string]bool{}
	for _, mv := range moves {
		moveSrc[mv.old] = true
	}

	// 7. 下行处理：遍历服务端变更路径（父先于子，保证目录改名连续性）
	downloads := map[string]protocol.NodeInfo{} // rel → 需写入的文件内容
	mkdirsLocal := map[string]bool{}
	stateDel := map[string]bool{}
	stateSet := map[string]Rec{}
	skipUpload := map[string]bool{}
	var movedPrefixes []struct{ old, new string } // 已应用的目录改名（后代磁盘随动）

	changedRels := make([]string, 0, len(changedSet))
	for rel := range changedSet {
		changedRels = append(changedRels, rel)
	}
	sort.Strings(changedRels)

	for _, rel := range changedRels {
		if excludedFn(rel) {
			continue // 选择性同步：排除子树不落地（FR-S9）
		}
		srec, sHas := serverNow[rel]
		lrec, lHas := baseline[rel]
		drec, dHas := scan[rel]
		_, isMod := modified[rel]
		_, isNew := newFiles[rel]
		isDel := del[rel]

		if sHas {
			oldRel, knownNode := baselineByNode[srec.ID]
			if knownNode {
				if np, moved := applyMovedPrefix(movedPrefixes, oldRel); moved {
					// 该节点随已改名的父目录一起移动，磁盘已就位
					stateDel[oldRel] = true
					stateSet[np] = recFromNode(srec)
					if np == rel {
						continue
					}
					// 路径与预期不符（父目录已动过）——按新位置记账后继续
					rel = np
				}
			}

			// 本地在此路径上有未上行的内容修改（modified/newFiles），优先走冲突/采纳
			localChangedHere := isMod || (isNew && knownNode && oldRel == rel)

			switch {
			case localChangedHere && srec.Type == protocol.TypeFile:
				// 双方都改了同一文件
				lh := modified[rel]
				if lh == "" {
					lh = newFiles[rel]
				}
				if lh == srec.ContentHash {
					// 内容一致：采纳服务端节点元数据
					skipUpload[rel] = true
					delete(modified, rel)
					delete(newFiles, rel)
					delete(del, rel)
					stateSet[rel] = recFromNode(srec)
					setLocalMTime(root, rel, srec.MTime)
				} else {
					// 冲突：本地版本留在原位（稍后上传），服务端版本存冲突副本（FR-S7）
					cc, err := e.conflictCopy(rel, srec, downloads, scan)
					if err != nil {
						return stats, err
					}
					stats.Conflicts++
					stateSet[cc] = Rec{Hash: srec.ContentHash, Size: srec.Size, MTime: srec.MTime, Type: protocol.TypeFile}
					newFiles[cc] = srec.ContentHash // 冲突副本随后上传
					scan[cc] = diskInfo{Size: srec.Size, MTime: srec.MTime}
					delete(del, rel)
				}

			case knownNode && oldRel != rel && !isDel && !localChangedHere:
				// 服务端移动/改名：本地跟随（rename 语义，不重传内容）
				oldBase := baseline[oldRel]
				oldPath := absJoin(root, oldRel)
				if dHas {
					// 目标路径本地仍有其他文件（未被服务端删除覆盖）：服务端版本存冲突副本，
					// 原文件交由其自身的变更条目处理，避免 rename 覆盖丢数据
					cc, err := e.conflictCopy(rel, srec, downloads, scan)
					if err != nil {
						return stats, err
					}
					stats.Conflicts++
					stateSet[cc] = Rec{Hash: srec.ContentHash, Size: srec.Size, MTime: srec.MTime, Type: protocol.TypeFile}
					newFiles[cc] = srec.ContentHash
					scan[cc] = diskInfo{Size: srec.Size, MTime: srec.MTime}
					continue
				}
				if err := os.MkdirAll(filepath.Dir(absJoin(root, rel)), 0o755); err != nil {
					return stats, err
				}
				if err := os.Rename(oldPath, absJoin(root, rel)); err != nil && !os.IsNotExist(err) {
					return stats, err
				}
				if srec.Type == protocol.TypeDir {
					movedPrefixes = append(movedPrefixes, struct{ old, new string }{oldRel, rel})
				} else {
					setLocalMTime(root, rel, srec.MTime)
				}
				stats.Moved++
				stateDel[oldRel] = true
				stateSet[rel] = recFromNode(srec)
				_ = oldBase

			case !dHas && knownNode:
				// 已知节点、本地旧位置无文件（本地已删除或已移走）
				if isDel && srec.Type == protocol.TypeFile && lrec.Hash != srec.ContentHash {
					// 本地删除 + 服务端修改：保留服务端版本为冲突副本（删除不生效）
					cc, err := e.conflictCopy(rel, srec, downloads, scan)
					if err != nil {
						return stats, err
					}
					stats.Conflicts++
					stateSet[cc] = Rec{Hash: srec.ContentHash, Size: srec.Size, MTime: srec.MTime, Type: protocol.TypeFile}
					newFiles[cc] = srec.ContentHash
					scan[cc] = diskInfo{Size: srec.Size, MTime: srec.MTime}
					delete(del, rel) // 保留服务端版本 → 不发送 unlink
					stateDel[rel] = true
				} else if isDel || moveSrc[oldRel] {
					// 双方删除一致，或本地移走且服务端未动（moveSrc 时内容由上传流程处理）
					// moveSrc + 服务端内容有变 → 冲突副本
					if moveSrc[oldRel] && srec.Type == protocol.TypeFile && lrec.Hash != srec.ContentHash {
						cc, err := e.conflictCopy(rel, srec, downloads, scan)
						if err != nil {
							return stats, err
						}
						stats.Conflicts++
						stateSet[cc] = Rec{Hash: srec.ContentHash, Size: srec.Size, MTime: srec.MTime, Type: protocol.TypeFile}
						newFiles[cc] = srec.ContentHash
						scan[cc] = diskInfo{Size: srec.Size, MTime: srec.MTime}
					}
					stateDel[rel] = true
				} else {
					// 本地缺失但未记录删除（异常，如上次同步中断）：恢复
					if srec.Type == protocol.TypeDir {
						mkdirsLocal[rel] = true
					} else {
						downloads[rel] = srec
					}
					stateSet[rel] = recFromNode(srec)
				}

			case !dHas && !knownNode:
				// 纯服务端新增
				if srec.Type == protocol.TypeDir {
					mkdirsLocal[rel] = true
				} else {
					downloads[rel] = srec
				}
				stateSet[rel] = recFromNode(srec)

			case dHas && !knownNode:
				// 本地新增与服务端新增撞路径
				if srec.Type == protocol.TypeDir || drec.IsDir {
					if srec.Type != protocol.TypeDir || !drec.IsDir {
						e.Log.Warn("类型冲突，跳过该路径（M1）", "path", rel)
						continue
					}
					mkdirsLocal[rel] = true
					stateSet[rel] = recFromNode(srec)
					continue
				}
				lh := newFiles[rel]
				if lh == "" {
					var err error
					lh, _, err = hashLocalFile(root, rel)
					if err != nil {
						return stats, err
					}
				}
				if lh == srec.ContentHash {
					skipUpload[rel] = true
					delete(newFiles, rel)
					stateSet[rel] = recFromNode(srec)
				} else {
					cc, err := e.conflictCopy(rel, srec, downloads, scan)
					if err != nil {
						return stats, err
					}
					stats.Conflicts++
					stateSet[cc] = Rec{Hash: srec.ContentHash, Size: srec.Size, MTime: srec.MTime, Type: protocol.TypeFile}
					newFiles[cc] = srec.ContentHash
					scan[cc] = diskInfo{Size: srec.Size, MTime: srec.MTime}
				}

			default:
				// 同一节点同路径（oldRel == rel），本地未改：与服务端对齐
				if lHas && srec.ContentHash == lrec.Hash && srec.Type == lrec.Type {
					stateSet[rel] = recFromNode(srec)
					setLocalMTime(root, rel, srec.MTime)
					delete(mtimeOnly, rel)
				} else if srec.Type == protocol.TypeDir {
					mkdirsLocal[rel] = true
					stateSet[rel] = recFromNode(srec)
				} else {
					downloads[rel] = srec
					stateSet[rel] = recFromNode(srec)
					delete(mtimeOnly, rel)
				}
			}
		} else {
			// 服务端已删除 rel
			if isDel || moveSrc[rel] {
				// 双方都删（或本地移走+服务端删除=移走生效）
				delete(del, rel)
				stateDel[rel] = true
			} else if dHas && lHas && !isMod && !isNew {
				// 本地未改：跟随删除
				os.RemoveAll(absJoin(root, rel))
				stateDel[rel] = true
				stats.Deleted++
			} else {
				// 本地有修改：本地胜出，重新上传（modified/newFiles 保持）
				stateDel[rel] = true
			}
		}
	}

	// 8. 执行本地写入（建目录 / 下载，写临时文件+原子改名，回设 mtime）
	for rel := range mkdirsLocal {
		if err := os.MkdirAll(absJoin(root, rel), 0o755); err != nil {
			return stats, err
		}
	}
	for rel, n := range downloads {
		abs := absJoin(root, rel)
		if err := os.MkdirAll(filepath.Dir(abs), 0o755); err != nil {
			return stats, err
		}
		if err := e.API.GetContent(n.ContentHash, abs, n.MTime); err != nil {
			return stats, fmt.Errorf("download %s: %w", rel, err)
		}
		h, size, err := hashLocalFile(root, rel)
		if err != nil {
			return stats, err
		}
		stateSet[rel] = Rec{NodeID: n.ID, Hash: h, Size: size, MTime: n.MTime, Type: protocol.TypeFile}
		scan[rel] = diskInfo{Size: size, MTime: n.MTime}
		stats.Downloaded++
	}

	// 9. 上行之内容上传（两阶段提交第一阶段；服务端按哈希去重）
	nodeAt := map[string]int64{} // rel → 服务端当前节点
	for rel, n := range serverNow {
		nodeAt[rel] = n.ID
	}
	createdDirs := map[string]int64{} // rel → 本次新建目录 node id

	type uploadItem struct {
		rel, hash string
	}
	var uploads []uploadItem
	for rel, h := range modified {
		if !skipUpload[rel] {
			uploads = append(uploads, uploadItem{rel, h})
		}
	}
	for rel, h := range newFiles {
		if !skipUpload[rel] {
			uploads = append(uploads, uploadItem{rel, h})
		}
	}
	chunkThreshold := e.Cfg.ChunkThresholdMB << 20
	chunkSize := e.Cfg.ChunkSizeMB << 20
	for _, up := range uploads {
		abs := absJoin(root, up.rel)
		var hash string
		if scan[up.rel].Size >= chunkThreshold {
			// FR-S11：大文件分块上传 + 断点续传
			sessID, _ := st.GetUploadSession(up.rel, up.hash)
			sid, h, err := e.API.PutContentChunked(abs, sessID, up.hash, scan[up.rel].Size, chunkSize)
			if sid != "" && err != nil {
				st.SetUploadSession(up.rel, up.hash, sid) // 保留会话供下次续传
			}
			if err != nil {
				return stats, fmt.Errorf("chunked upload %s: %w", up.rel, err)
			}
			st.ClearUploadSession(up.rel, up.hash)
			hash = h
		} else {
			var err error
			hash, _, _, err = e.API.PutContent(abs)
			if err != nil {
				return stats, fmt.Errorf("upload %s: %w", up.rel, err)
			}
		}
		if hash != up.hash {
			return stats, fmt.Errorf("upload %s: hash changed during sync", up.rel)
		}
		stats.Uploaded++
	}

	// 10. 上行之元数据操作（顺序：mkdir → unlink → move → put，服务端按序原子应用）
	// 崩溃恢复标记：ops 提交到状态持久化之间若进程死亡，下次启动将重建状态
	writePendingMarker(root)

	// 10a. mkdir（本地新增目录）
	var mkdirOps []protocol.Op
	var mkdirRels []string
	for rel := range localDirs {
		if nodeAt[rel] != 0 {
			continue
		}
		parentID, pname, ok := e.parentFor(rel, f, nodeAt, createdDirs)
		if !ok {
			continue
		}
		mkdirOps = append(mkdirOps, protocol.Op{Op: protocol.OpMkdir, ParentID: parentID, Name: pname})
		mkdirRels = append(mkdirRels, rel)
	}
	if len(mkdirOps) > 0 {
		res, err := e.API.Ops(mkdirOps)
		if err != nil {
			return stats, err
		}
		for i := range mkdirRels {
			if i < len(res) && res[i].Ok {
				createdDirs[mkdirRels[i]] = res[i].NodeID
				stateSet[mkdirRels[i]] = Rec{NodeID: res[i].NodeID, Type: protocol.TypeDir}
			} else if i < len(res) {
				e.Log.Warn("mkdir failed", "path", mkdirRels[i], "err", res[i].Error)
			}
		}
	}

	// 10b. unlink（先深后浅；服务端目录删除是递归的，幂等）
	var delRels []string
	for rel := range del {
		if nodeAt[rel] != 0 {
			delRels = append(delRels, rel)
		} else {
			stateDel[rel] = true
		}
	}
	sort.Slice(delRels, func(i, j int) bool { return depth(delRels[i]) > depth(delRels[j]) })
	if len(delRels) > 0 {
		ops := make([]protocol.Op, len(delRels))
		for i, rel := range delRels {
			ops[i] = protocol.Op{Op: protocol.OpUnlink, NodeID: nodeAt[rel]}
		}
		res, err := e.API.Ops(ops)
		if err != nil {
			return stats, err
		}
		for i := range delRels {
			if i < len(res) && res[i].Ok {
				stateDel[delRels[i]] = true
				stats.Deleted++
			} else if i < len(res) {
				e.Log.Warn("unlink failed", "path", delRels[i], "err", res[i].Error)
			}
		}
	}

	// 10c. move（目标被占/祖先被删时降级为删除+重建上传）
	var moveOps []protocol.Op
	var movePlans []moveRec
	var demoted []string
	for _, mv := range moves {
		nodeID := nodeAt[mv.old]
		if nodeID == 0 || underAny(delRels, mv.old) || underAny(delRels, parentOf(mv.new)) {
			demoted = append(demoted, mv.new)
			stateDel[mv.old] = true
			continue
		}
		parentID, pname, ok := e.parentFor(mv.new, f, nodeAt, createdDirs)
		if !ok {
			demoted = append(demoted, mv.new)
			stateDel[mv.old] = true
			continue
		}
		moveOps = append(moveOps, protocol.Op{Op: protocol.OpMove, NodeID: nodeID, ParentID: parentID, Name: pname})
		movePlans = append(movePlans, mv)
	}
	if len(moveOps) > 0 {
		res, err := e.API.Ops(moveOps)
		if err != nil {
			return stats, err
		}
		for i := range movePlans {
			if i < len(res) && res[i].Ok {
				stateDel[movePlans[i].old] = true
				d := scan[movePlans[i].new]
				if d.IsDir {
					stateSet[movePlans[i].new] = Rec{NodeID: nodeAt[movePlans[i].old], Type: protocol.TypeDir}
				} else {
					stateSet[movePlans[i].new] = Rec{NodeID: nodeAt[movePlans[i].old], Hash: movePlans[i].hash,
						Size: d.Size, MTime: d.MTime, Type: protocol.TypeFile}
				}
				stats.Moved++
			} else if i < len(res) {
				e.Log.Warn("move failed", "path", movePlans[i].old, "err", res[i].Error)
				demoted = append(demoted, movePlans[i].new)
			}
		}
	}
	for _, rel := range demoted {
		if d, ok := scan[rel]; ok && !d.IsDir {
			h, err := hashOfLocal(root, rel)
			if err != nil {
				return stats, err
			}
			newFiles[rel] = h
		}
	}

	// 10d. put：mtimeOnly → modified → newFiles（NodeID 优先复用服务端现存节点）
	var putOps []protocol.Op
	var putRels []string
	var putHash []string
	var putMTime []int64
	addPut := func(rel, hash string, nodeID int64) {
		if nodeID == 0 {
			nodeID = nodeAt[rel] // 服务端当前在 rel 上的节点（冲突覆盖场景）
		}
		parentID, pname, ok := e.parentFor(rel, f, nodeAt, createdDirs)
		if !ok {
			e.Log.Warn("缺少父目录，跳过上传", "path", rel)
			return
		}
		if nodeID > 0 {
			putOps = append(putOps, protocol.Op{Op: protocol.OpPut, NodeID: nodeID,
				ContentHash: hash, Size: scan[rel].Size, MTime: scan[rel].MTime, Name: pname})
		} else {
			putOps = append(putOps, protocol.Op{Op: protocol.OpPut, ParentID: parentID, Name: pname,
				ContentHash: hash, Size: scan[rel].Size, MTime: scan[rel].MTime})
		}
		putRels = append(putRels, rel)
		putHash = append(putHash, hash)
		putMTime = append(putMTime, scan[rel].MTime)
	}
	for rel := range mtimeOnly {
		if skipUpload[rel] {
			continue
		}
		addPut(rel, baseline[rel].Hash, baseline[rel].NodeID)
	}
	for rel, h := range modified {
		if skipUpload[rel] {
			continue
		}
		addPut(rel, h, 0)
	}
	for rel, h := range newFiles {
		if skipUpload[rel] {
			continue
		}
		addPut(rel, h, 0)
	}
	if len(putOps) > 0 {
		res, err := e.API.Ops(putOps)
		if err != nil {
			return stats, err
		}
		for i := range putRels {
			if i < len(res) && res[i].Ok {
				stateSet[putRels[i]] = Rec{NodeID: res[i].NodeID, Hash: putHash[i],
					Size: scan[putRels[i]].Size, MTime: putMTime[i], Type: protocol.TypeFile}
			} else if i < len(res) {
				e.Log.Warn("put failed", "path", putRels[i], "err", res[i].Error)
			}
		}
	}

	// 11. 持久化状态与新 cursor（FR-S14：每文件夹独立 cursor）
	for rel := range stateDel {
		st.Delete(rel)
	}
	for rel, r := range stateSet {
		st.Set(rel, r)
	}
	if err := st.SetCursor(newCursor); err != nil {
		return stats, err
	}
	f.Cursor = newCursor
	SaveConfig(e.Cfg)
	clearPendingMarker(root)
	return stats, nil
}

// ---------- 辅助 ----------

func (e *Engine) resolveRoot(f *Folder) (int64, error) {
	nodes, err := e.API.Nodes()
	if err != nil {
		return 0, err
	}
	for _, n := range nodes {
		if n.Path == f.Name && n.Type == protocol.TypeDir {
			return n.ID, nil
		}
	}
	res, err := e.API.Ops([]protocol.Op{{Op: protocol.OpMkdir, Name: f.Name}})
	if err != nil {
		return 0, err
	}
	if len(res) == 1 && res[0].Ok {
		return res[0].NodeID, nil
	}
	return 0, fmt.Errorf("resolve root %q: %v", f.Name, res)
}

// parentFor 返回 rel 父目录的 node id 与 rel 自身的名字。
func (e *Engine) parentFor(rel string, f *Folder, nodeAt, createdDirs map[string]int64) (int64, string, bool) {
	dir, name := splitDir(rel)
	if dir == "" {
		return f.RootNodeID, name, true
	}
	if id, ok := createdDirs[dir]; ok {
		return id, name, true
	}
	if id, ok := nodeAt[dir]; ok && id != 0 {
		return id, name, true
	}
	return 0, "", false
}

// conflictCopy 把服务端版本下载为冲突副本（FR-S7 命名：`name (conflict from 设备名).ext`）。
func (e *Engine) conflictCopy(rel string, srec protocol.NodeInfo, downloads map[string]protocol.NodeInfo, scan map[string]diskInfo) (string, error) {
	if srec.Type == protocol.TypeDir {
		return "", fmt.Errorf("cannot conflict-copy dir %s", rel)
	}
	dir, name := splitDir(rel)
	ext := filepath.Ext(name)
	base := strings.TrimSuffix(name, ext)
	mk := func(i int) string {
		n := fmt.Sprintf("%s (conflict from %s)%s", base, e.Cfg.DeviceName, ext)
		if i > 1 {
			n = fmt.Sprintf("%s (conflict from %s) %d%s", base, e.Cfg.DeviceName, i, ext)
		}
		if dir != "" {
			return dir + "/" + n
		}
		return n
	}
	cc := mk(1)
	for i := 2; ; i++ {
		_, takenScan := scan[cc]
		_, takenDl := downloads[cc]
		if !takenScan && !takenDl {
			break
		}
		cc = mk(i)
	}
	downloads[cc] = srec
	return cc, nil
}

// detectMoves：删除集与新增集中同哈希同大小的对 → move（FR-S6，避免删除+重传）。
func detectMoves(del map[string]bool, newFiles map[string]string, baseline map[string]Rec, scan map[string]diskInfo) []moveRec {
	var moves []moveRec
	for dRel, dRec := range baseline {
		if dRec.Type != protocol.TypeFile || !del[dRel] {
			continue
		}
		for nRel, h := range newFiles {
			if h != dRec.Hash || scan[nRel].Size != dRec.Size {
				continue
			}
			moves = append(moves, moveRec{old: dRel, new: nRel, hash: h})
			delete(del, dRel)
			delete(newFiles, nRel)
			break
		}
	}
	return moves
}

func recFromNode(n protocol.NodeInfo) Rec {
	return Rec{NodeID: n.ID, Hash: n.ContentHash, Size: n.Size, MTime: n.MTime, Type: n.Type}
}

func absJoin(root, rel string) string {
	return filepath.Join(root, filepath.FromSlash(rel))
}

func splitDir(rel string) (string, string) {
	i := strings.LastIndex(rel, "/")
	if i < 0 {
		return "", rel
	}
	return rel[:i], rel[i+1:]
}

func parentOf(rel string) string { d, _ := splitDir(rel); return d }

func depth(rel string) int { return strings.Count(rel, "/") }

func underAny(set []string, rel string) bool {
	if rel == "" {
		return false
	}
	for _, s := range set {
		if s == rel || strings.HasPrefix(rel, s+"/") {
			return true
		}
	}
	return false
}

func hasDescendant(scan map[string]diskInfo, dirRel string) bool {
	prefix := dirRel + "/"
	for rel := range scan {
		if strings.HasPrefix(rel, prefix) {
			return true
		}
	}
	return false
}

func applyMovedPrefix(moved []struct{ old, new string }, rel string) (string, bool) {
	for _, m := range moved {
		if rel == m.old || strings.HasPrefix(rel, m.old+"/") {
			return m.new + strings.TrimPrefix(rel, m.old), true
		}
	}
	return "", false
}

func setLocalMTime(root, rel string, mtimeMilli int64) {
	if mtimeMilli <= 0 {
		return
	}
	t := time.UnixMilli(mtimeMilli)
	os.Chtimes(absJoin(root, rel), t, t)
}

func hashOfLocal(root, rel string) (string, error) {
	h, _, err := hashLocalFile(root, rel)
	return h, err
}

// LoadIgnore 根层规则：默认清单（FR-S17）+ .syncignore +（可选）.gitignore。
func LoadIgnore(root string, useGitignore bool) *Ignore {
	patterns := append([]string{}, defaultPatterns...)
	if b, err := os.ReadFile(filepath.Join(root, ".syncignore")); err == nil {
		patterns = append(patterns, strings.Split(string(b), "\n")...)
	}
	if useGitignore {
		if b, err := os.ReadFile(filepath.Join(root, ".gitignore")); err == nil {
			patterns = append(patterns, strings.Split(string(b), "\n")...)
		}
	}
	return NewIgnore(patterns)
}

// ---------- 崩溃恢复标记 ----------

func pendingMarkerPath(root string) string { return filepath.Join(root, ".y-sync", "pending.json") }

func pendingMarkerExists(root string) bool {
	_, err := os.Stat(pendingMarkerPath(root))
	return err == nil
}

func writePendingMarker(root string) {
	os.MkdirAll(filepath.Join(root, ".y-sync"), 0o755)
	os.WriteFile(pendingMarkerPath(root), []byte(fmt.Sprintf("{\"note\":\"ops in flight\",\"ts\":%d}", time.Now().Unix())), 0o600)
}

func clearPendingMarker(root string) { os.Remove(pendingMarkerPath(root)) }

func resetStateDB(root string) {
	sp := StatePath(root)
	for _, suffix := range []string{"", "-wal", "-shm"} {
		os.Remove(sp + suffix)
	}
}
