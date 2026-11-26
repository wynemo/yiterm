// yiterm-tailcat exposes exactly one local YiTerm TLS port. The parent owns
// its lifetime via stdin; stdout is a single private JSON readiness message.
package main

import (
	"context"
	"encoding/json"
	"flag"
	"fmt"
	"io"
	"net"
	"net/url"
	"os"
	"os/signal"
	"strconv"
	"sync"
	"time"

	"github.com/skip2/go-qrcode"
	"github.com/tailscale/tailcat"
)

func main() {
	port := flag.Int("port", 0, "Local YiTerm TLS port")
	flag.Parse()
	if *port < 1 || *port > 65535 {
		os.Exit(2)
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt)
	defer stop()
	go func() { io.Copy(io.Discard, os.Stdin); stop() }()
	var mu sync.Mutex
	connections := map[net.Conn]bool{}
	closed := false
	track := func(c net.Conn) bool {
		mu.Lock()
		defer mu.Unlock()
		if closed {
			c.Close()
			return false
		}
		connections[c] = true
		return true
	}
	release := func(c net.Conn) { c.Close(); mu.Lock(); delete(connections, c); mu.Unlock() }
	server := &tailcat.Server{Logf: func(string, ...any) {}}
	server.OnTCP = func(p uint16) func(net.Conn) {
		if int(p) != *port {
			return nil
		}
		return func(incoming net.Conn) {
			if !track(incoming) {
				return
			}
			defer release(incoming)
			local, err := (&net.Dialer{Timeout: 10 * time.Second}).DialContext(ctx, "tcp", net.JoinHostPort("127.0.0.1", strconv.Itoa(*port)))
			if err != nil {
				return
			}
			if !track(local) {
				return
			}
			defer release(local)
			tailcat.ProxyConns(incoming, local)
		}
	}
	startup, cancel := context.WithTimeout(ctx, 25*time.Second)
	err := server.StartContext(startup)
	cancel()
	if err != nil {
		fmt.Fprintln(os.Stderr, "Cannot start encrypted sharing relay")
		os.Exit(1)
	}
	defer server.Close()
	hostname, _ := os.Hostname()
	values := url.Values{"v": {"1"}, "address": {string(server.TailcatAddr())}, "port": {strconv.Itoa(*port)}, "name": {hostname}, "mode": {"standard"}}
	link := "tenimon://yiterm/connect?" + values.Encode()
	qr, err := qrcode.New(link, qrcode.Medium)
	if err != nil {
		os.Exit(1)
	}
	rows := []string{}
	for _, row := range qr.Bitmap() {
		line := make([]byte, len(row))
		for i, dark := range row {
			line[i] = '0'
			if dark {
				line[i] = '1'
			}
		}
		rows = append(rows, string(line))
	}
	if err := json.NewEncoder(os.Stdout).Encode(struct {
		URL  string   `json:"url"`
		Rows []string `json:"rows"`
	}{link, rows}); err != nil {
		return
	}
	<-ctx.Done()
	mu.Lock()
	closed = true
	for c := range connections {
		c.Close()
	}
	mu.Unlock()
}
