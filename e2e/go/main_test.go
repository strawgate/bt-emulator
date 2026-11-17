// Runs the official Braintrust Go SDK against a real bt-emulator process.
//
// BT_EMULATOR_URL: use a running emulator. Otherwise BT_EMULATOR_BIN (default
// target/debug/bt-emulator) is started on an ephemeral port with in-memory SQLite
// (override with BT_EMULATOR_DATABASE_URL), code execution enabled, and prompts
// running against a local stub model.
package e2e

import (
	"bufio"
	"bytes"
	"crypto/rand"
	"encoding/hex"
	"encoding/json"
	"fmt"
	"io"
	"net"
	"net/http"
	"os"
	"os/exec"
	"path/filepath"
	"strings"
	"testing"
)

const apiKey = "bt-emulator-local"

var emulatorURL string

func TestMain(m *testing.M) {
	url, stop, err := start()
	if err != nil {
		fmt.Fprintln(os.Stderr, err)
		os.Exit(1)
	}
	emulatorURL = url
	code := m.Run()
	stop()
	os.Exit(code)
}

func start() (string, func(), error) {
	if url := os.Getenv("BT_EMULATOR_URL"); url != "" {
		return strings.TrimSuffix(url, "/"), func() {}, nil
	}
	model, err := startStubModel()
	if err != nil {
		return "", nil, err
	}
	bin := os.Getenv("BT_EMULATOR_BIN")
	if bin == "" {
		bin, _ = filepath.Abs("../../target/debug/bt-emulator")
	}
	db := os.Getenv("BT_EMULATOR_DATABASE_URL")
	if db == "" {
		db = "sqlite::memory:"
	}
	cmd := exec.Command(bin, "serve", "--listen", "127.0.0.1:0", "--database-url", db,
		"--llm-base-url", model.URL, "--allow-code-execution")
	cmd.Stderr = os.Stderr
	stdout, err := cmd.StdoutPipe()
	if err != nil {
		return "", nil, err
	}
	if err := cmd.Start(); err != nil {
		return "", nil, err
	}
	line, err := bufio.NewReader(stdout).ReadString('\n')
	if err != nil || !strings.HasPrefix(line, "listening on ") {
		return "", nil, fmt.Errorf("emulator failed to start: %q %v", line, err)
	}
	stop := func() {
		_ = cmd.Process.Signal(os.Interrupt)
		_ = cmd.Wait()
		model.Close()
	}
	return strings.TrimSpace(strings.TrimPrefix(line, "listening on ")), stop, nil
}

type stubModel struct {
	URL   string
	Close func()
}

// startStubModel serves a deterministic OpenAI-compatible model: it echoes the last
// message, and an LLM classifier picks the choice named in the last message.
func startStubModel() (*stubModel, error) {
	ln, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return nil, err
	}
	handler := http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		var req struct {
			Messages []struct {
				Content string `json:"content"`
			} `json:"messages"`
			Tools []struct {
				Function struct {
					Name       string `json:"name"`
					Parameters struct {
						Properties struct {
							Choice struct {
								Enum []string `json:"enum"`
							} `json:"choice"`
						} `json:"properties"`
					} `json:"parameters"`
				} `json:"function"`
			} `json:"tools"`
		}
		_ = json.NewDecoder(r.Body).Decode(&req)
		last := req.Messages[len(req.Messages)-1].Content
		var message map[string]any
		if len(req.Tools) > 0 && req.Tools[0].Function.Name == "select_choice" {
			choices := req.Tools[0].Function.Parameters.Properties.Choice.Enum
			choice := choices[0]
			for _, c := range choices {
				if strings.Contains(last, c) {
					choice = c
				}
			}
			args, _ := json.Marshal(map[string]string{"choice": choice, "reasoning": "stub"})
			message = map[string]any{"role": "assistant", "tool_calls": []any{map[string]any{
				"id": "1", "type": "function", "function": map[string]any{"name": "select_choice", "arguments": string(args)},
			}}}
		} else {
			message = map[string]any{"role": "assistant", "content": "echo: " + last}
		}
		w.Header().Set("Content-Type", "application/json")
		_ = json.NewEncoder(w).Encode(map[string]any{"choices": []any{map[string]any{"index": 0, "message": message, "finish_reason": "stop"}}})
	})
	srv := &http.Server{Handler: handler}
	go func() { _ = srv.Serve(ln) }()
	return &stubModel{URL: "http://" + ln.Addr().String() + "/v1", Close: func() { _ = srv.Close() }}, nil
}

// rest calls the emulator's REST API directly, to set up and read back state.
func rest(t *testing.T, method, path string, body any) map[string]any {
	t.Helper()
	var reader io.Reader
	if body != nil {
		data, _ := json.Marshal(body)
		reader = bytes.NewReader(data)
	}
	req, _ := http.NewRequest(method, emulatorURL+path, reader)
	req.Header.Set("Authorization", "Bearer "+apiKey)
	req.Header.Set("Content-Type", "application/json")
	resp, err := http.DefaultClient.Do(req)
	if err != nil {
		t.Fatal(err)
	}
	defer resp.Body.Close()
	data, _ := io.ReadAll(resp.Body)
	if resp.StatusCode != http.StatusOK {
		t.Fatalf("%s %s: %d %s", method, path, resp.StatusCode, data)
	}
	var out map[string]any
	if err := json.Unmarshal(data, &out); err != nil {
		t.Fatalf("%s %s: %v: %s", method, path, err, data)
	}
	return out
}

func events(t *testing.T, objectType, id string) []map[string]any {
	t.Helper()
	var out []map[string]any
	for _, e := range rest(t, "POST", "/v1/"+objectType+"/"+id+"/fetch", map[string]any{})["events"].([]any) {
		out = append(out, e.(map[string]any))
	}
	return out
}

func projectName() string {
	b := make([]byte, 6)
	_, _ = rand.Read(b)
	return "e2e-go-" + hex.EncodeToString(b)
}
