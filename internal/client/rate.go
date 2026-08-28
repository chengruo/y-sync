// 令牌桶限速读写器（FR-S12）：bytes/sec 粒度，突发上限为 1 秒的配额。
package client

import (
	"io"
	"sync"
	"time"
)

type rateLimiter struct {
	mu     sync.Mutex
	rate   int64 // bytes/sec，<=0 表示不限
	tokens float64
	last   time.Time
}

func newRateLimiter(rate int64) *rateLimiter {
	return &rateLimiter{rate: rate, tokens: float64(rate), last: time.Now()}
}

func (r *rateLimiter) take(n int64) {
	if r == nil || r.rate <= 0 {
		return
	}
	for {
		r.mu.Lock()
		now := time.Now()
		r.tokens += now.Sub(r.last).Seconds() * float64(r.rate)
		if r.tokens > float64(r.rate) {
			r.tokens = float64(r.rate)
		}
		r.last = now
		if r.tokens >= float64(n) {
			r.tokens -= float64(n)
			r.mu.Unlock()
			return
		}
		need := (float64(n) - r.tokens) / float64(r.rate)
		r.mu.Unlock()
		time.Sleep(time.Duration(need * float64(time.Second)))
	}
}

// rateReader 下载限速。
type rateReader struct {
	r  io.Reader
	rl *rateLimiter
}

func (x *rateReader) Read(p []byte) (int, error) {
	n, err := x.r.Read(p)
	if n > 0 {
		x.rl.take(int64(n))
	}
	return n, err
}

type writeCounter struct {
	w io.Writer
	n int64
}

func (c *writeCounter) Write(p []byte) (int, error) {
	n, err := c.w.Write(p)
	c.n += int64(n)
	return n, err
}
