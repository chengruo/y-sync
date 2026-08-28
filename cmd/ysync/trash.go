package main

import (
	"fmt"
	"path/filepath"
	"time"

	"ysync/internal/client"
)

// ysync trash list / restore <id> / rm <id>
func cmdTrash(args []string) error {
	if len(args) == 0 {
		return fmt.Errorf("usage: ysync trash list | restore <id> | rm <id>")
	}
	cfg, err := client.LoadConfig()
	if err != nil {
		return err
	}
	api := client.NewAPI(cfg.ServerURL, cfg.Token)
	switch args[0] {
	case "list":
		items, err := api.TrashList()
		if err != nil {
			return err
		}
		if len(items) == 0 {
			fmt.Println("(回收站为空)")
			return nil
		}
		for _, it := range items {
			kind := "file"
			if it.Type == "dir" {
				kind = "dir "
			}
			fmt.Printf("  %-8d %s %-60s %8.1fKB  删除于 %s\n",
				it.ID, kind, it.OrigPath, float64(it.Size)/1024,
				time.Unix(it.DeletedAt, 0).Format("2006-01-02 15:04"))
		}
	case "restore":
		if len(args) != 2 {
			return fmt.Errorf("usage: ysync trash restore <id>")
		}
		var id int64
		if _, err := fmt.Sscanf(args[1], "%d", &id); err != nil {
			return err
		}
		if err := api.TrashRestore(id); err != nil {
			return err
		}
		fmt.Printf("已恢复 #%d（各设备将在下次同步取回）\n", id)
	case "rm":
		if len(args) != 2 {
			return fmt.Errorf("usage: ysync trash rm <id>")
		}
		var id int64
		if _, err := fmt.Sscanf(args[1], "%d", &id); err != nil {
			return err
		}
		if err := api.TrashDelete(id); err != nil {
			return err
		}
		fmt.Printf("已彻底删除 #%d\n", id)
	default:
		return fmt.Errorf("未知子命令 %q", args[0])
	}
	return nil
}

// ysync versions list <folder> <relpath> / restore <folder> <relpath> <version-id>
func cmdVersions(args []string) error {
	if len(args) < 2 {
		return fmt.Errorf("usage: ysync versions list <folder> <relpath> | restore <folder> <relpath> <version-id>")
	}
	cfg, err := client.LoadConfig()
	if err != nil {
		return err
	}
	api := client.NewAPI(cfg.ServerURL, cfg.Token)
	var folder *client.Folder
	for i := range cfg.Folders {
		if cfg.Folders[i].Name == args[1] {
			folder = &cfg.Folders[i]
		}
	}
	if folder == nil {
		return fmt.Errorf("文件夹 %q 不存在", args[1])
	}
	if len(args) < 3 {
		return fmt.Errorf("缺少相对路径")
	}
	rel := args[2]
	nodeID, err := lookupNode(api, folder, rel)
	if err != nil {
		return err
	}
	switch args[0] {
	case "list":
		versions, err := api.NodeVersions(nodeID)
		if err != nil {
			return err
		}
		if len(versions) == 0 {
			fmt.Println("(无历史版本)")
			return nil
		}
		for _, v := range versions {
			fmt.Printf("  %-8d %-64s %8.1fKB  %s\n",
				v.ID, v.Hash[:16]+"…", float64(v.Size)/1024,
				time.Unix(v.Created, 0).Format("2006-01-02 15:04:05"))
		}
	case "restore":
		if len(args) != 4 {
			return fmt.Errorf("usage: ysync versions restore <folder> <relpath> <version-id>")
		}
		var vid int64
		if _, err := fmt.Sscanf(args[3], "%d", &vid); err != nil {
			return err
		}
		dest := filepath.Join(folder.LocalPath, filepath.FromSlash(rel))
		if err := api.DownloadVersionTo(vid, dest, 0); err != nil {
			return err
		}
		fmt.Printf("版本 #%d 已写回 %s（下次 sync 上行）\n", vid, rel)
	default:
		return fmt.Errorf("未知子命令 %q", args[0])
	}
	return nil
}

// lookupNode 由文件夹内相对路径查服务端 node_id。
func lookupNode(api *client.API, f *client.Folder, rel string) (int64, error) {
	nodes, err := api.Nodes()
	if err != nil {
		return 0, err
	}
	target := f.Name + "/" + rel
	for _, n := range nodes {
		if n.Path == target {
			return n.ID, nil
		}
	}
	return 0, fmt.Errorf("服务端不存在 %q", rel)
}
