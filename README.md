# cpphxbuilder

This was completely vibe-coded by claude with a single prompt.

A terminal TUI that builds your C++/clang project, shows a scrollable list of
diagnostics, and lets you jump directly into Helix at the exact error location —
then returns you to the TUI when you quit.

## Quick start

```sh
# From your project root (where compile_commands.json lives):
cpphxbuilder
```

The tool will run `cd ./build && cmake --build . -- -j 64`, stream the output
into both a log file and the TUI, parse every diagnostic line, and present them
in an interactive list.

## Keybindings

| Key               | Action                                      |
|-------------------|---------------------------------------------|
| `↑` / `k`         | Move selection up                           |
| `↓` / `j`         | Move selection down                         |
| `PgUp` / `PgDn`   | Scroll by a page                            |
| `Home`            | Jump to first diagnostic / top of log       |
| `End`             | Jump to last diagnostic / bottom of log     |
| `Enter`           | Open selected location in `hx`, return after|
| `r`               | Rebuild (re-runs the build command)         |
| `Tab`             | Toggle between Diagnostics and Log views    |
| `f`               | Toggle filter: all diagnostics / errors only|
| `q` / `Ctrl-c`    | Quit                                        |

## Environment variables

| Variable          | Default                            | Description                       |
|-------------------|------------------------------------|-----------------------------------|
| `CPPHX_BUILD_DIR` | `./build`                          | Directory passed to `cd` before the build command |
| `CPPHX_BUILD_CMD` | `cmake --build . -- -j 64`         | The build command itself           |
| `CPPHX_LOG_PATH`  | `cpphxbuilder.log`                 | Where the raw build output goes    |

### Example: custom build directory and parallelism

```sh
CPPHX_BUILD_DIR=./build-debug CPPHX_BUILD_CMD="cmake --build . -- -j 8" cpphxbuilder
```

### Example: using Ninja

```sh
CPPHX_BUILD_CMD="ninja -j 64" cpphxbuilder
```

## Building from source

Requires Rust 1.75 or later.

```sh
cargo build --release
# Binary at: target/release/cpphxbuilder
cp target/release/cpphxbuilder ~/.local/bin/   # or wherever your PATH points
```

## What it parses

The tool understands GCC/Clang diagnostic lines (`file:line:col: severity: message`)
and MSVC-style lines (`file(line,col): severity Cxxxx: message`). ANSI escape codes
from `-fdiagnostics-color=always` are stripped before matching. Up to 8 lines of
compiler-emitted context (code snippet + caret) are attached to each diagnostic and
shown in the detail panel.

## How the Helix integration works

Pressing Enter:
1. Tears down the TUI (leaves alternate screen)
2. Runs `hx path:line:col` in the same terminal process
3. When you quit `hx` (`q` / `:q`), the TUI is restored

No IPC, no multiplexer required. Works in any terminal.
