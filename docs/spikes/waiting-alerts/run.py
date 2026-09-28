"""Fill an alert prompt template and run it through `claude -p --model haiku`, printing the output and wall time.

Usage: python run.py <prompt file> <message file> <session name> [lean]
"lean" adds flags that skip user/project settings (hooks, plugins), MCP servers and tools, to measure startup cost.
"""

import subprocess
import sys
import time
from pathlib import Path

here = Path(__file__).parent
template = (here / sys.argv[1]).read_text(encoding="utf-8")
message = (here / sys.argv[2]).read_text(encoding="utf-8")
session = sys.argv[3]
lean = len(sys.argv) > 4 and sys.argv[4] == "lean"
prompt = template.replace("{{SESSION}}", session).replace("{{REASON}}", "turn finished").replace("{{MESSAGE}}", message.strip())
argv = ["claude", "-p", "--model", "haiku", "--no-session-persistence"]
if lean:
    argv += ["--setting-sources", "", "--strict-mcp-config", "--tools", ""]
start = time.monotonic()
result = subprocess.run(argv, input=prompt, capture_output=True, text=True, encoding="utf-8", check=False, cwd=here)
print(result.stdout.strip())
if result.returncode != 0:
    print("STDERR:", result.stderr.strip()[:500])
print(f"-- {sys.argv[2]}{' lean' if lean else ''}: exit {result.returncode}, {time.monotonic() - start:.1f}s")
