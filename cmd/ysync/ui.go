package main

import (
	"fmt"
	"os/exec"
	"runtime"

	"ysync/internal/client"
)

// cmdUI 打开本地管理台：读取 daemon.json 获取地址与 token，调起系统浏览器。
func cmdUI() error {
	info, err := client.ReadDaemonInfo()
	if err != nil {
		return err
	}
	url := fmt.Sprintf("http://%s/?token=%s", info.Addr, info.Token)
	var cmd *exec.Cmd
	switch runtime.GOOS {
	case "darwin":
		cmd = exec.Command("open", url)
	case "windows":
		cmd = exec.Command("rundll32", "url.dll,FileProtocolHandler", url)
	default:
		cmd = exec.Command("xdg-open", url)
	}
	if err := cmd.Start(); err != nil {
		// 打不开浏览器就直接给出链接
		fmt.Println(url)
		return nil
	}
	fmt.Printf("已在浏览器打开管理台: %s\n", url)
	return nil
}

// cmdRemove 解除跟踪文件夹（FR-S15：本地文件与服务端副本都保留）。
// 复用 daemon 的实现：加载配置 → 调 RemoveFolder 逻辑。
func cmdRemove(args []string) error {
	if len(args) != 1 {
		return fmt.Errorf("usage: ysync remove <name>")
	}
	cfg, err := client.LoadConfig()
	if err != nil {
		return err
	}
	idx := -1
	for i := range cfg.Folders {
		if cfg.Folders[i].Name == args[0] {
			idx = i
			break
		}
	}
	if idx < 0 {
		return fmt.Errorf("文件夹 %q 不存在", args[0])
	}
	cfg.Folders = append(cfg.Folders[:idx], cfg.Folders[idx+1:]...)
	if err := client.SaveConfig(cfg); err != nil {
		return err
	}
	fmt.Printf("已解除跟踪 %q（本地文件与服务端副本保留；如需恢复请重新 ysync add）\n", args[0])
	return nil
}
