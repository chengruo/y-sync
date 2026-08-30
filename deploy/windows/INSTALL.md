# Windows 服务端部署（NSSM 方式）

1. 下载 `ysync-server-rs-windows-amd64.exe`（GitHub Release）到 `C:\y-sync\`
2. 创建数据目录 `C:\y-sync\data`
3. 用 NSSM 注册服务（https://nssm.cc/download）：

```powershell
nssm install y-sync "C:\y-sync\ysync-server-rs.exe" "serve -addr 127.0.0.1:8720 -data C:\y-sync\data"
nssm set y-sync AppDirectory C:\y-sync
nssm set y-sync DisplayName "y-sync Server"
nssm set y-sync Start SERVICE_AUTO_START
nssm start y-sync
```

4. 建首个用户：
```powershell
$env:YSYNC_DATA="C:\y-sync\data"
"你的密码" | C:\y-sync\ysync-server-rs.exe adduser alice
```

5. IIS/nginx 反代到 127.0.0.1:8720（参考 deploy/nginx-y-sync.conf.example 的
   WebSocket 头与 client_max_body_size 0 两项）。

卸载：`nssm stop y-sync && nssm remove y-sync confirm`
