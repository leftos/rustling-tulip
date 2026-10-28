"""Run the alert prompt over a sample folder through Haiku, DeepSeek or a local llama-server, one JSON line per call.

Usage:
  uv run --no-project python -I bench.py haiku <samples dir> <out.jsonl> [--repeat N] [--no-thinking] [--prompt FILE]
  uv run --no-project python -I bench.py deepseek <samples dir> <out.jsonl> [--repeat N]
  uv run --no-project python -I bench.py local <samples dir> <out.jsonl> --base-url URL [--model NAME] [--repeat N]
      [--schema] [--thinking off|low|on]
  uv run --no-project python -I bench.py local <samples dir> <out.jsonl> --serve <model.gguf> --server <llama-server.exe>
      --ngl N [--ctx 8192] [--port P] [--server-log FILE] [...the options above]

`--prompt` names the template (default: `prompt.md` beside this script, the v2 text); `{{TRIGGER}}` becomes `stop`
and `{{MESSAGE}}` the sample, in that order, as the daemon builds it.

`haiku` pipes the prompt to the Claude Code CLI headless (`claude -p --model claude-haiku-4-5 --effort low
--output-format json`, the CLI's lowest effort, with user settings, MCP servers, tools and slash commands off and no
session saved, answering in the daemon's `SUMMARY_SCHEMA` read from `crates/daemon/src/summarizer/reply.rs` through
`--json-schema`: the daemon's own argv), never the API directly. The reply text is the CLI's `structured_output` as
JSON when it has one, else its `result`, as the daemon reads it; the line keeps the CLI's `total_cost_usd`,
`duration_api_ms` and usage. `--effort low` alone still lets Haiku think (about 1,700 output tokens a call);
`--no-thinking` also sets `MAX_THINKING_TOKENS=0`, which turns thinking off.

`deepseek` posts to DeepSeek's Anthropic-compatible Messages endpoint (`deepseek-flash`, default effort,
`max_tokens` 1500), the request the design's summarizer makes. The key comes from DEEPSEEK_API_KEY in the process
environment or the Windows user-scope registry and is never printed.

`local` posts to an OpenAI-compatible `/v1/chat/completions` (temperature 0, `max_tokens` 400, thinking off unless
`--thinking` says otherwise). With `--serve` the script starts llama-server itself (`-c`, `-ngl`, `-np 1`, `--jinja`,
no web UI), waits for `/health`, records load time, VRAM (nvidia-smi `memory.used` minus the pre-start reading) and the
server's working set and private bytes, runs every call, then stops the server it started. The first call after load
is marked `cold`. `--schema` sends the alert's JSON schema as `response_format` (llama.cpp turns it into a grammar).

Each call line holds the sample id, round, wall seconds, the reply text, llama-server's `timings` or DeepSeek's
`usage`, and an `error` on failure. A `server` line (with `--serve`) holds the load and memory figures.
"""

import argparse
import json
import os
import re
import subprocess
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

HERE = Path(__file__).resolve().parent
REPLY_RS = HERE.parents[2] / "crates" / "daemon" / "src" / "summarizer" / "reply.rs"
DEEPSEEK_URL = "https://api.deepseek.com/anthropic/v1/messages"
DEEPSEEK_MODEL = "deepseek-flash"
HAIKU_MODEL = "claude-haiku-4-5"
CALL_TIMEOUT_S = 60
KINDS = ["needs_answer", "working_update", "done_waiting"]
SCHEMA = {
    "type": "object",
    "properties": {
        "kind": {"type": "string", "enum": KINDS},
        "headline": {"type": "string"},
        "speech": {"type": "string"},
        "notification": {"type": "string"},
    },
    "required": ["kind", "headline", "speech", "notification"],
    "additionalProperties": False,
}


def summary_schema() -> str:
    """Return the daemon's `SUMMARY_SCHEMA` text, read from reply.rs so the bench sends what the daemon sends."""
    found = re.search(r'SUMMARY_SCHEMA: &str = r#"(.*?)"#;', REPLY_RS.read_text(encoding="utf-8"))
    if not found:
        raise SystemExit(f"no SUMMARY_SCHEMA in {REPLY_RS}")
    return found.group(1)


def api_key(var: str) -> str:
    """Return the key from the environment, falling back to the user-scope registry on Windows."""
    key = os.environ.get(var)
    if key:
        return key
    if sys.platform == "win32":
        import winreg

        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, "Environment") as env:
            value, _ = winreg.QueryValueEx(env, var)
            return str(value)
    raise SystemExit(f"{var} is not set in the environment or HKCU\\Environment")


def post_json(url: str, body: dict, headers: dict[str, str]) -> dict:
    """POST a JSON body and return the decoded JSON reply; raise RuntimeError with the status and body on failure."""
    request = urllib.request.Request(url, data=json.dumps(body).encode(), headers={"content-type": "application/json", **headers})
    try:
        with urllib.request.urlopen(request, timeout=CALL_TIMEOUT_S) as response:
            return json.loads(response.read())
    except urllib.error.HTTPError as error:
        raise RuntimeError(f"HTTP {error.code}: {error.read()[:300]!r}") from error


def call_deepseek(prompt: str, key: str) -> tuple[str, dict]:
    """One Messages call to DeepSeek Flash; return the reply text and the usage block."""
    body = {"model": DEEPSEEK_MODEL, "max_tokens": 1500, "messages": [{"role": "user", "content": prompt}]}
    reply = post_json(DEEPSEEK_URL, body, {"x-api-key": key, "anthropic-version": "2023-06-01"})
    text = "".join(block.get("text", "") for block in reply.get("content", []) if block.get("type") == "text")
    return text, {"usage": reply.get("usage"), "blocks": sorted({block.get("type") for block in reply.get("content", [])})}


def call_haiku(prompt: str, args: argparse.Namespace) -> tuple[str, dict]:
    """One headless Claude Code call to Haiku at low effort; return the result text and the CLI's cost and timing."""
    argv = [
        "claude", "-p", "--model", HAIKU_MODEL, "--effort", "low", "--output-format", "json", "--no-session-persistence",
        "--setting-sources", "", "--strict-mcp-config", "--tools", "", "--disable-slash-commands",
        "--json-schema", args.json_schema,
    ]  # fmt: skip
    env = {**os.environ, "MAX_THINKING_TOKENS": "0"} if args.no_thinking else None
    result = subprocess.run(argv, input=prompt, capture_output=True, text=True, encoding="utf-8", check=False, cwd=HERE, timeout=180, env=env)
    if result.returncode != 0:
        raise RuntimeError(f"claude exited {result.returncode}: {result.stderr.strip()[:300]}")
    reply = json.loads(result.stdout)
    if reply.get("is_error"):
        raise RuntimeError(f"claude reported an error: {str(reply.get('result'))[:300]}")
    keep = ("total_cost_usd", "duration_api_ms", "duration_ms", "usage", "modelUsage")
    extra = {key: reply.get(key) for key in keep}
    structured = reply.get("structured_output")
    extra["structured"] = structured is not None
    return (json.dumps(structured) if structured is not None else reply.get("result") or ""), extra


def call_local(prompt: str, args: argparse.Namespace) -> tuple[str, dict]:
    """One chat-completions call to an OpenAI-compatible server; return the reply text and llama-server's timings."""
    body: dict = {
        "model": args.model,
        "messages": [{"role": "user", "content": prompt}],
        "temperature": 0,
        "max_tokens": 400 if args.thinking == "off" else 2000,
        "chat_template_kwargs": {"enable_thinking": args.thinking != "off", "low_effort": args.thinking == "low"},
    }
    if args.schema:
        body["response_format"] = {"type": "json_schema", "json_schema": {"name": "alert", "strict": True, "schema": SCHEMA}}
    reply = post_json(f"{args.base_url}/v1/chat/completions", body, {})
    message = reply["choices"][0]["message"]
    extra = {"timings": reply.get("timings"), "usage": reply.get("usage"), "finish": reply["choices"][0].get("finish_reason")}
    if message.get("reasoning_content"):
        extra["reasoning_chars"] = len(message["reasoning_content"])
    return message.get("content") or "", extra


def vram_used_mib() -> int:
    """Return the GPU's total used memory in MiB from nvidia-smi."""
    out = subprocess.run(["nvidia-smi", "--query-gpu=memory.used", "--format=csv,noheader,nounits"], capture_output=True, text=True, check=True)
    return int(out.stdout.split()[0])


def process_memory_mib(pid: int) -> dict[str, float]:
    """Return a process's working set and private bytes in MiB."""
    command = f'$p = Get-Process -Id {pid}; "$($p.WorkingSet64) $($p.PrivateMemorySize64)"'
    out = subprocess.run(["pwsh", "-NoProfile", "-Command", command], capture_output=True, text=True, check=True)
    working_set, private = (int(value) for value in out.stdout.split())
    return {"working_set_mib": round(working_set / 2**20), "private_mib": round(private / 2**20)}


def start_server(args: argparse.Namespace) -> tuple[subprocess.Popen, dict]:
    """Start llama-server, wait until it answers /health, and return it with its load and memory figures."""
    vram_before = vram_used_mib()
    argv = [
        args.server,
        "-m",
        str(args.serve),
        "-c",
        str(args.ctx),
        "-ngl",
        str(args.ngl),
        "-np",
        "1",
        "--jinja",
        "--no-webui",
        "--host",
        "127.0.0.1",
        "--port",
        str(args.port),
    ]
    log = open(args.server_log, "w", encoding="utf-8") if args.server_log else subprocess.DEVNULL  # noqa: SIM115 - closed with the server
    start = time.monotonic()
    server = subprocess.Popen(argv, stdout=log, stderr=subprocess.STDOUT)
    print(f"started llama-server pid {server.pid}", flush=True)
    while True:
        if server.poll() is not None:
            raise SystemExit(f"llama-server exited with {server.returncode} before it was ready; see {args.server_log}")
        try:
            with urllib.request.urlopen(f"{args.base_url}/health", timeout=2) as response:
                if response.status == 200:
                    break
        except (urllib.error.URLError, ConnectionError, TimeoutError):
            pass
        if time.monotonic() - start > 300:
            server.terminate()
            raise SystemExit("llama-server was not ready within 300 s")
        time.sleep(0.2)
    figures = {"load_s": round(time.monotonic() - start, 2), "argv": argv[1:], "pid": server.pid}
    figures["vram_mib"] = vram_used_mib() - vram_before
    figures.update(process_memory_mib(server.pid))
    return server, figures


def stop_server(server: subprocess.Popen) -> None:
    """Stop the llama-server this script started and wait for it to exit."""
    server.terminate()
    try:
        server.wait(timeout=30)
    except subprocess.TimeoutExpired:
        server.kill()
        server.wait(timeout=30)
    print(f"stopped llama-server pid {server.pid} (exit {server.returncode})", flush=True)


def call_provider(args: argparse.Namespace, prompt: str, key: str) -> tuple[str, dict]:
    if args.provider == "deepseek":
        return call_deepseek(prompt, key)
    if args.provider == "haiku":
        return call_haiku(prompt, args)
    return call_local(prompt, args)


def run_calls(args: argparse.Namespace, out) -> None:
    """Run every sample `--repeat` times and write one JSON line per call."""
    samples = sorted(args.samples.glob("*.md"))
    key = api_key("DEEPSEEK_API_KEY") if args.provider == "deepseek" else ""
    template = args.prompt.read_text(encoding="utf-8").replace("{{TRIGGER}}", "stop")
    first = True
    for round_index in range(args.repeat):
        for sample in samples:
            prompt = template.replace("{{MESSAGE}}", sample.read_text(encoding="utf-8").strip())
            line: dict = {"sample": sample.stem, "round": round_index, "cold": first}
            start = time.monotonic()
            try:
                text, extra = call_provider(args, prompt, key)
                line.update(text=text, **extra)
            except (RuntimeError, urllib.error.URLError, TimeoutError, KeyError, subprocess.TimeoutExpired, json.JSONDecodeError) as error:
                line["error"] = str(error)
            line["wall_s"] = round(time.monotonic() - start, 3)
            first = False
            out.write(json.dumps(line) + "\n")
            out.flush()
            print(f"{sample.stem} r{round_index}: {line['wall_s']:.2f}s{' ERROR ' + line['error'] if 'error' in line else ''}", flush=True)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser()
    parser.add_argument("provider", choices=["haiku", "deepseek", "local"])
    parser.add_argument("samples", type=Path)
    parser.add_argument("out", type=Path)
    parser.add_argument("--repeat", type=int, default=1)
    parser.add_argument("--prompt", type=Path, default=HERE / "prompt.md", help="the prompt template (default: the v2 prompt.md here)")
    parser.add_argument("--base-url", default="http://127.0.0.1:18427")
    parser.add_argument("--model", default="local")
    parser.add_argument("--schema", action="store_true")
    parser.add_argument("--no-thinking", action="store_true", help="haiku: set MAX_THINKING_TOKENS=0 for the CLI")
    parser.add_argument("--thinking", choices=["off", "low", "on"], default="off")
    parser.add_argument("--serve", type=Path)
    parser.add_argument("--server", default="llama-server")
    parser.add_argument("--ngl", type=int, default=99)
    parser.add_argument("--ctx", type=int, default=8192)
    parser.add_argument("--port", type=int, default=18427)
    parser.add_argument("--server-log", type=Path)
    args = parser.parse_args()
    if args.serve:
        args.base_url = f"http://127.0.0.1:{args.port}"
    args.json_schema = summary_schema() if args.provider == "haiku" else ""
    return args


def main() -> None:
    args = parse_args()
    args.out.parent.mkdir(parents=True, exist_ok=True)
    server = None
    with args.out.open("w", encoding="utf-8", newline="\n") as out:
        try:
            if args.serve:
                server, figures = start_server(args)
                out.write(json.dumps({"server": figures}) + "\n")
                print(f"server ready: {figures}", flush=True)
            run_calls(args, out)
            if server:
                out.write(json.dumps({"server_after": process_memory_mib(server.pid)}) + "\n")
        finally:
            if server:
                stop_server(server)


if __name__ == "__main__":
    main()
