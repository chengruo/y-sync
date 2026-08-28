// daemon 运行时状态：文件夹同步状态、暂停集合、最近统计（控制 API / 状态页数据源）。
package client

import (
	"sync"
	"time"
)

// FolderStatus 单文件夹最近一次同步的状态。
type FolderStatus struct {
	Name      string    `json:"name"`
	LocalPath string    `json:"local_path"`
	Cursor    int64     `json:"cursor"`
	Files     int       `json:"files"`
	LastSync  time.Time `json:"last_sync,omitempty"`
	LastError string    `json:"last_error,omitempty"`
	Conflicts int       `json:"conflicts_total"`
	Paused    bool      `json:"paused"`
	LastStats string    `json:"last_stats,omitempty"`
}

// DaemonState daemon 运行时的可变状态（线程安全）。
type DaemonState struct {
	mu      sync.Mutex
	paused  map[string]bool
	status  map[string]*FolderStatus
	version string
}

func NewDaemonState() *DaemonState {
	return &DaemonState{paused: map[string]bool{}, status: map[string]*FolderStatus{}}
}

func (d *DaemonState) InitFolders(folders []Folder) {
	d.mu.Lock()
	defer d.mu.Unlock()
	for i := range folders {
		f := folders[i]
		if _, ok := d.status[f.Name]; !ok {
			d.status[f.Name] = &FolderStatus{Name: f.Name, LocalPath: f.LocalPath, Cursor: f.Cursor}
		}
	}
}

func (d *DaemonState) Pause(name string) {
	d.mu.Lock()
	defer d.mu.Unlock()
	if name == "" {
		for k := range d.paused {
			d.paused[k] = true
		}
		for k := range d.status {
			d.paused[k] = true
		}
		return
	}
	d.paused[name] = true
	if s := d.status[name]; s != nil {
		s.Paused = true
	}
}

func (d *DaemonState) Resume(name string) {
	d.mu.Lock()
	defer d.mu.Unlock()
	if name == "" {
		d.paused = map[string]bool{}
		for k := range d.status {
			d.status[k].Paused = false
		}
		return
	}
	delete(d.paused, name)
	if s := d.status[name]; s != nil {
		s.Paused = false
	}
}

func (d *DaemonState) IsPaused(name string) bool {
	d.mu.Lock()
	defer d.mu.Unlock()
	return d.paused[name]
}

// BeginSync 记录一次同步开始（返回 false 表示该文件夹已暂停）。
func (d *DaemonState) BeginSync(name string) bool {
	d.mu.Lock()
	defer d.mu.Unlock()
	return !d.paused[name]
}

func (d *DaemonState) FinishSync(f *Folder, files int, stats string) {
	d.mu.Lock()
	defer d.mu.Unlock()
	s := d.status[f.Name]
	if s == nil {
		s = &FolderStatus{Name: f.Name, LocalPath: f.LocalPath}
		d.status[f.Name] = s
	}
	s.LastSync = time.Now()
	s.Cursor = f.Cursor
	s.Files = files
	s.LastError = ""
	s.LastStats = stats
}

func (d *DaemonState) FailSync(f *Folder, err error) {
	d.mu.Lock()
	defer d.mu.Unlock()
	s := d.status[f.Name]
	if s == nil {
		s = &FolderStatus{Name: f.Name, LocalPath: f.LocalPath}
		d.status[f.Name] = s
	}
	s.LastSync = time.Now()
	s.LastError = err.Error()
}

func (d *DaemonState) AddConflicts(name string, n int) {
	d.mu.Lock()
	defer d.mu.Unlock()
	if s := d.status[name]; s != nil {
		s.Conflicts += n
	}
}

func (d *DaemonState) Snapshot() []FolderStatus {
	d.mu.Lock()
	defer d.mu.Unlock()
	out := make([]FolderStatus, 0, len(d.status))
	for _, s := range d.status {
		out = append(out, *s)
	}
	return out
}
