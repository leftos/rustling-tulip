"""Run an alert prompt through the Messages API on Haiku 4.5 and report the reply, latency, tokens and cost.

Usage: uv run --with anthropic python api.py <prompt file> <message file>...
The key is read from ANTHROPIC_API_KEY_TOAST (process environment, else the Windows user-scope registry).
"""

import os
import sys
import time
from pathlib import Path

import anthropic

MODEL = "claude-haiku-4-5"
INPUT_PER_TOKEN = 1.00 / 1_000_000
OUTPUT_PER_TOKEN = 5.00 / 1_000_000
KEY_VAR = "ANTHROPIC_API_KEY_TOAST"


def api_key() -> str:
    """Return the alert API key from the environment, falling back to the user-scope registry on Windows."""
    key = os.environ.get(KEY_VAR)
    if key:
        return key
    if sys.platform == "win32":
        import winreg

        with winreg.OpenKey(winreg.HKEY_CURRENT_USER, "Environment") as env:
            value, _ = winreg.QueryValueEx(env, KEY_VAR)
            return str(value)
    raise SystemExit(f"{KEY_VAR} is not set")


def main() -> None:
    here = Path(__file__).parent
    template = (here / sys.argv[1]).read_text(encoding="utf-8")
    client = anthropic.Anthropic(api_key=api_key())
    for name in sys.argv[2:]:
        message = (here / name).read_text(encoding="utf-8").strip()
        prompt = template.replace("{{SESSION}}", "Next up plan index").replace("{{MESSAGE}}", message)
        start = time.monotonic()
        response = client.messages.create(model=MODEL, max_tokens=400, messages=[{"role": "user", "content": prompt}])
        wall = time.monotonic() - start
        text = "".join(block.text for block in response.content if block.type == "text")
        usage = response.usage
        cost = usage.input_tokens * INPUT_PER_TOKEN + usage.output_tokens * OUTPUT_PER_TOKEN
        print(text.strip())
        print(f"-- {name}: {wall:.2f}s, {usage.input_tokens} in / {usage.output_tokens} out, ${cost:.5f}\n")


if __name__ == "__main__":
    main()
