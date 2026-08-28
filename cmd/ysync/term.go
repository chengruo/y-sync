package main

import (
	"bufio"
	"fmt"
	"os"

	"golang.org/x/term"
)

func readPassword() (string, error) {
	if term.IsTerminal(int(os.Stdin.Fd())) {
		b, err := term.ReadPassword(int(os.Stdin.Fd()))
		fmt.Println()
		return string(b), err
	}
	// 非终端（脚本/测试）：从标准输入读整行
	sc := bufio.NewScanner(os.Stdin)
	if !sc.Scan() {
		return "", fmt.Errorf("read password failed")
	}
	return sc.Text(), nil
}
