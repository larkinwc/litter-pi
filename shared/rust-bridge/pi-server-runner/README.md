# pi-server-runner

Headless host runner for exercising the in-process pi coding-agent
runtime end-to-end without booting iOS. Today it ships the `--local`
mode, which spins up the `PtyDevToolFactory` (macOS host shell — the
dev/CI stand-in for iSH on iOS), sends a prompt, and prints every
`PiEvent` as a JSONL transcript on stdout.

## Usage

```sh
./target/release/pi-server-runner --local --provider anthropic --prompt 'hi'
```

CLI:

* `--local` — required; only supported mode in this milestone.
* `--prompt <text>` *or* `--stdin` — prompt source.
* `--provider <id>` — explicit pi provider (e.g. `anthropic`,
  `openai`). Defaults to a heuristic based on the env keys present.
* `--model <id>` — override the default model.
* `--timeout-secs <n>` — wall-clock budget for the turn (default `180`).
* `--tool-factory <kind>` — built-in tool factory to mount on the
  in-process runtime. Today the only accepted value is `pty-dev`
  (default), which threads the macOS host-shell factory through the
  same `start_in_process` path used by the iOS iSH and Android proot
  factories. The runner logs `pi-server-runner: tool_factory=<kind>`
  to stderr at startup so validators can confirm the right factory
  was wired in.

## BYOK environment variables

The runner reads BYOK credentials and optional endpoint overrides from
the environment before pi loads:

| Var                  | Purpose                                                     |
| -------------------- | ----------------------------------------------------------- |
| `ANTHROPIC_API_KEY`  | Selects the Anthropic provider when no `--provider` is set. |
| `ANTHROPIC_BASE_URL` | Custom Anthropic-compatible endpoint (e.g. a BYOK proxy).   |
| `OPENAI_API_KEY`     | Falls back to the OpenAI-compatible provider.               |
| `OPENAI_BASE_URL`    | Custom OpenAI-compatible endpoint.                          |

### Base-URL precedence

The base-URL env var matching the **active provider** wins:

* `--provider anthropic` → `ANTHROPIC_BASE_URL` is forwarded into
  `PiSessionConfig.base_url`. `OPENAI_BASE_URL` is ignored even if
  set.
* `--provider openai` → `OPENAI_BASE_URL` is forwarded into
  `PiSessionConfig.base_url`. `ANTHROPIC_BASE_URL` is ignored even if
  set.
* When `--provider` is omitted, the provider is chosen from whichever
  `*_API_KEY` is set, and the matching `*_BASE_URL` is applied.

If neither base URL is set the upstream provider default is used
(`api.anthropic.com` or `api.openai.com`).

## Exit codes

* `0` — runtime emitted `PiEvent::TurnComplete`.
* `1` — runtime emitted `PiEvent::TurnError`, or the broadcast channel
  closed before any turn-terminal event arrived, or the timeout fired.
* `2` — CLI/runtime could not start (missing prompt, missing BYOK
  credentials, IO error, etc.).
