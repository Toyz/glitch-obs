package main

import (
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"fmt"
	"log"
	"os"
	"strconv"
	"time"

	"github.com/gorilla/websocket"
)

// obs-websocket 5.x opcodes
const (
	opHello    = 0
	opIdentify = 1
	opReady    = 2
	opRequest  = 6
	opResponse = 7
)

type wsMsg struct {
	Op int             `json:"op"`
	D  json.RawMessage `json:"d"`
}

type helloData struct {
	ObsWebSocketVersion string `json:"obsWebSocketVersion"`
	RPCVersion          int    `json:"rpcVersion"`
	Authentication      *struct {
		Challenge string `json:"challenge"`
		Salt      string `json:"salt"`
	} `json:"authentication"`
}

// Global --source flag for targeting a specific OBS source.
var targetSource string

func main() {
	log.SetFlags(0)

	// Pull --source from args (anywhere in the list).
	args := extractSourceFlag(os.Args[1:])

	if len(args) < 1 {
		usage()
		os.Exit(1)
	}

	addr := envOr("OBS_WS_ADDR", "localhost:4455")
	pass := os.Getenv("OBS_WS_PASSWORD")

	conn := connect(addr, pass)
	defer conn.Close()

	cmd := args[0]
	switch cmd {
	case "pulse":
		expr := argFromOr(args, 1, "c ^ 128")
		dur := argIntFromOr(args, 2, 5000)
		data := map[string]interface{}{
			"expression":  expr,
			"duration_ms": dur,
		}
		maybeSetSource(data)
		resp := vendorRequest(conn, "pulse", data)
		fmt.Println("pulse:", pretty(resp))

	case "set_expression":
		expr := argFromOr(args, 1, "c & 200")
		data := map[string]interface{}{
			"expression": expr,
		}
		maybeSetSource(data)
		resp := vendorRequest(conn, "set_expression", data)
		fmt.Println("set_expression:", pretty(resp))

	case "set_enabled":
		enabled := argFromOr(args, 1, "true") == "true"
		data := map[string]interface{}{
			"enabled": enabled,
		}
		maybeSetSource(data)
		resp := vendorRequest(conn, "set_enabled", data)
		fmt.Println("set_enabled:", pretty(resp))

	case "get_state":
		data := map[string]interface{}{}
		maybeSetSource(data)
		resp := vendorRequest(conn, "get_state", data)
		fmt.Println("get_state:", pretty(resp))

	default:
		log.Fatalf("unknown command: %s\n", cmd)
	}
}

func usage() {
	fmt.Fprintf(os.Stderr, `Usage: ws-client [--source "Source Name"] <command> [args...]

Commands:
  pulse [expression] [duration_ms]   Apply glitch for N ms then revert
  set_expression [expression]        Permanently change expression
  set_enabled [true|false]           Enable/disable the filter
  get_state                          Get current filter state

Options:
  --source "Name"  Target a specific OBS source (omit to broadcast to all)

Environment:
  OBS_WS_ADDR      WebSocket address  (default: localhost:4455)
  OBS_WS_PASSWORD   WebSocket password (default: none)

Examples:
  ws-client pulse "c ^ 128" 3000
  ws-client --source "Camera" pulse "c ^ 128" 3000
  ws-client get_state
`)
}

// extractSourceFlag removes --source "name" from args and sets targetSource.
func extractSourceFlag(args []string) []string {
	var out []string
	for i := 0; i < len(args); i++ {
		if args[i] == "--source" && i+1 < len(args) {
			targetSource = args[i+1]
			i++ // skip value
		} else {
			out = append(out, args[i])
		}
	}
	return out
}

func maybeSetSource(data map[string]interface{}) {
	if targetSource != "" {
		data["source"] = targetSource
	}
}

// ─── WebSocket connection + obs-websocket 5.x handshake ─────────

func connect(addr, password string) *websocket.Conn {
	url := fmt.Sprintf("ws://%s", addr)
	conn, _, err := websocket.DefaultDialer.Dial(url, nil)
	if err != nil {
		log.Fatalf("dial %s: %v", url, err)
	}

	// Read Hello (op 0)
	hello := readMsg(conn)
	if hello.Op != opHello {
		log.Fatalf("expected Hello (op 0), got op %d", hello.Op)
	}

	var hd helloData
	json.Unmarshal(hello.D, &hd)
	log.Printf("connected to obs-websocket %s (rpc v%d)", hd.ObsWebSocketVersion, hd.RPCVersion)

	// Build Identify
	identify := map[string]interface{}{
		"rpcVersion":         1,
		"eventSubscriptions": 0,
	}

	if hd.Authentication != nil {
		if password == "" {
			log.Fatal("server requires authentication — set OBS_WS_PASSWORD")
		}
		identify["authentication"] = computeAuth(password, hd.Authentication.Salt, hd.Authentication.Challenge)
	}

	writeMsg(conn, opIdentify, identify)

	// Read Identified (op 2)
	ready := readMsg(conn)
	if ready.Op != opReady {
		log.Fatalf("expected Identified (op 2), got op %d: %s", ready.Op, string(ready.D))
	}
	log.Println("identified successfully")
	return conn
}

func computeAuth(password, salt, challenge string) string {
	h1 := sha256.Sum256([]byte(password + salt))
	b64h1 := base64.StdEncoding.EncodeToString(h1[:])
	h2 := sha256.Sum256([]byte(b64h1 + challenge))
	return base64.StdEncoding.EncodeToString(h2[:])
}

// ─── Vendor request helper ──────────────────────────────────────

var reqID int

func vendorRequest(conn *websocket.Conn, reqType string, data map[string]interface{}) map[string]interface{} {
	reqID++
	id := fmt.Sprintf("glitch-%d-%d", time.Now().UnixMilli(), reqID)

	writeMsg(conn, opRequest, map[string]interface{}{
		"requestType": "CallVendorRequest",
		"requestId":   id,
		"requestData": map[string]interface{}{
			"vendorName":  "glitch",
			"requestType": reqType,
			"requestData": data,
		},
	})

	resp := readMsg(conn)
	if resp.Op != opResponse {
		log.Fatalf("expected Response (op 7), got op %d", resp.Op)
	}

	var body map[string]interface{}
	json.Unmarshal(resp.D, &body)
	return body
}

// ─── Low-level helpers ──────────────────────────────────────────

func readMsg(conn *websocket.Conn) wsMsg {
	var m wsMsg
	if err := conn.ReadJSON(&m); err != nil {
		log.Fatalf("read: %v", err)
	}
	return m
}

func writeMsg(conn *websocket.Conn, op int, d interface{}) {
	raw, _ := json.Marshal(d)
	msg := map[string]interface{}{
		"op": op,
		"d":  json.RawMessage(raw),
	}
	if err := conn.WriteJSON(msg); err != nil {
		log.Fatalf("write: %v", err)
	}
}

func pretty(v interface{}) string {
	b, _ := json.MarshalIndent(v, "", "  ")
	return string(b)
}

func envOr(key, fallback string) string {
	if v := os.Getenv(key); v != "" {
		return v
	}
	return fallback
}

func argFromOr(args []string, i int, fallback string) string {
	if len(args) > i {
		return args[i]
	}
	return fallback
}

func argIntFromOr(args []string, i int, fallback int) int {
	if len(args) > i {
		v, err := strconv.Atoi(args[i])
		if err == nil {
			return v
		}
	}
	return fallback
}
