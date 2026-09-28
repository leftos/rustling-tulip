"""Run an alert prompt through an Anthropic-compatible Messages API and report the reply, latency, tokens and cost.

Usage: uv run --with anthropic python api.py <haiku|deepseek> <effort or -> <prompt file> <message file>...
Keys: ANTHROPIC_API_KEY_TOAST (haiku), DEEPSEEK_API_KEY (deepseek), from the process environment or the Windows
user-scope registry. Effort `-` sends none; any other value is sent as `output_config.effort`.
"""

import os
import sys
import time
from dataclasses import dataclass
from pathlib import Path

import anthropic


@dataclass(frozen=True)
class Backend:
    model: str
    base_url: str | None
    key_var: str
    input_per_mtok: float
    output_per_mtok: float


BACKENDS = {
    "haiku": Backend("claude-haiku-4-5", None, "ANTHROPIC_API_KEY_TOAST", 1.00, 5.00),
    # DeepSeek's peak list price; off-peak is half.
    "deepseek": Backend("deepseek-flash", "https://api.deepseek.com/anthropic", "DEEPSEEK_API_KEY", 0.30, 1.20),
}


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
    raise SystemExit(f"{var} is not set")


def main() -> None:
    backend = BACKENDS[sys.argv[1]]
    effort = sys.argv[2]
    here = Path(__file__).parent
    template = (here / sys.argv[3]).read_text(encoding="utf-8")
    client = anthropic.Anthropic(api_key=api_key(backend.key_var), base_url=backend.base_url)
    extra = {} if effort == "-" else {"output_config": {"effort": effort}}
    for name in sys.argv[4:]:
        message = (here / name).read_text(encoding="utf-8").strip()
        prompt = template.replace("{{SESSION}}", "Next up plan index").replace("{{MESSAGE}}", message)
        start = time.monotonic()
        response = client.messages.create(
            model=backend.model, max_tokens=2000, messages=[{"role": "user", "content": prompt}], extra_body=extra
        )
        wall = time.monotonic() - start
        text = "".join(block.text for block in response.content if block.type == "text")
        kinds = sorted({block.type for block in response.content})
        usage = response.usage
        cost = (usage.input_tokens * backend.input_per_mtok + usage.output_tokens * backend.output_per_mtok) / 1_000_000
        print(text.strip())
        print(f"-- {name}: {wall:.2f}s, {usage.input_tokens} in / {usage.output_tokens} out, ${cost:.5f}, blocks {kinds}\n")


if __name__ == "__main__":
    main()
