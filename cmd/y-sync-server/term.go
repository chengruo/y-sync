package main

import (
	"bufio"
	"fmt"
	"os"

	"golang.org/x/term"
)

func readLine() (string, error) {
	sc := bufio.NewScanner(os.Stdin)
	if !sc.Scan() {
		return "", fmt.Errorf("read failed")
	}
	return sc.Text(), nil
}

// 密码从终端安全读取；非终端（脚本/测试）时回退到标准输入整行。
func readPassword() (string, error) {
	if term.IsTerminal(int(os.Stdin.Fd())) {
		b, err := term.ReadPassword(int(os.Stdin.Fd()))
		fmt.Println()
		return string(b), err
	}
	return readLine()
}
