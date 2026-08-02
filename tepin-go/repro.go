package tepin

import (
	"encoding/json"
	"strconv"
	"strings"
)

// Ops with a CLI twin: op name -> argument order for the command line.
var cliShapes = map[string][]string{
	"inspect": {},
	"query":   {"collection", "filter"},
	"get":     {"collection", "id"},
	"insert":  {"collection", "doc"},
	"upsert":  {"collection", "doc"},
	"update":  {"collection", "id", "doc"},
	"delete":  {"collection", "id"},
	"purpose": {"collection", "text"},
	"search":  {"query"},
}

func shellQuote(s string) string {
	return "'" + strings.ReplaceAll(s, "'", `'\''`) + "'"
}

// cliRepro renders the `tepin …` command equivalent to an op, or "" when
// it has none (in-memory handles, primitives-tier ops).
func cliRepro(path, op string, args any) string {
	shape, ok := cliShapes[op]
	if !ok || path == "" {
		return ""
	}
	argMap := map[string]any{}
	if raw, err := json.Marshal(args); err == nil {
		_ = json.Unmarshal(raw, &argMap)
	}
	parts := []string{"tepin", strings.ReplaceAll(op, "_", "-"), path}
	for _, key := range shape {
		v, present := argMap[key]
		if !present || v == nil {
			continue
		}
		if s, isStr := v.(string); isStr {
			parts = append(parts, shellQuote(s))
		} else if raw, err := json.Marshal(v); err == nil {
			parts = append(parts, shellQuote(string(raw)))
		}
	}
	if op == "search" {
		if c, _ := argMap["collection"].(string); c != "" {
			parts = append(parts, "--collection", c)
		}
		if l, isNum := argMap["limit"].(float64); isNum && l > 0 {
			parts = append(parts, "--limit", strconv.Itoa(int(l)))
		}
	}
	return strings.Join(parts, " ")
}
