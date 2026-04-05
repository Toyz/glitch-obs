# Glitch - OBS Studio Filter Plugin

Real-time pixel glitch effects for OBS Studio, powered by an expression engine.

Write math expressions that transform pixel values on the fly. Control the filter live via the OBS WebSocket API -- perfect for Twitch bots, Stream Deck, or any automation.

## Features

- **Expression engine** -- pixel manipulation via math expressions (bitwise ops, arithmetic, neighbor sampling, randomness)
- **GPU staging** -- stages GPU textures to CPU for processing, uploads back; works with display/window/game capture
- **Async source support** -- direct CPU frame processing for webcams, PipeWire, media sources
- **Parallel processing** -- splits pixel work across all CPU cores via rayon
- **Frame skipping** -- configurable processing interval to balance quality vs performance
- **WebSocket API** -- control the filter externally via obs-websocket vendor requests
- **Timed pulses** -- apply a glitch expression for N milliseconds then auto-revert
- **Multi-instance routing** -- target specific sources when multiple filters are active

## Installation

### From release

Download the binary for your platform from [Releases](../../releases) and copy it to your OBS plugins directory:

| Platform | Plugin file | Install path |
|----------|------------|--------------|
| Linux | `libobs_glitch.so` | `/usr/lib/obs-plugins/` |
| macOS | `libobs_glitch.dylib` | `~/Library/Application Support/obs-studio/plugins/` |
| Windows | `obs_glitch.dll` | `C:\Program Files\obs-studio\obs-plugins\64bit\` |

Restart OBS after copying.

### From source (Linux)

```bash
# Requires: Rust toolchain, libobs-dev, pkg-config
cargo build --release
sudo cp target/release/libobs_glitch.so /usr/lib/obs-plugins/
```

Or use the install script:

```bash
./install.sh
```

## Usage

1. Open OBS Studio
2. Right-click a video source > **Filters**
3. Click **+** > **Glitch**
4. Edit the expression in the properties panel

### Expression syntax

Expressions are mapped over every pixel's color components, returning a new value (0-255). Powered by [glitch-core](https://github.com/Toyz/glitch).

**Try expressions live at [theglitch.ing](https://theglitch.ing)** to preview the output before using them in OBS.

#### Parameters

| Param | Description |
|-------|-------------|
| `c` | Current pixel component value |
| `b` | Blurred version of `c` |
| `h` | Horizontally flipped `c` |
| `v` | Vertically flipped `c` |
| `d` | Diagonally flipped `c` |
| `Y` | Luminosity / grayscale component |
| `N` | Noise pixel (random value per component) |
| `R` | Red channel (rgb 255,0,0) |
| `G` | Green channel (rgb 0,255,0) |
| `B` | Blue channel (rgb 0,0,255) |
| `s` | Value from the last saved expression evaluation |
| `r` | Random component from neighboring 8 pixels |
| `e` | Edge detect -- difference of pixels in a box |
| `x` | Current x coordinate, normalized to 0-255 |
| `y` | Current y coordinate, normalized to 0-255 |
| `H` | Highest component in neighboring 8 pixels |
| `L` | Lowest component in neighboring 8 pixels |

#### Custom operators

| Syntax | Description |
|--------|-------------|
| `t` | Random component from neighboring 16 pixels |
| `g` | Random component from random locations in image |
| `r{N}` | Random component from neighboring N pixels |
| `R{N}`, `G{N}`, `B{N}` | Color channel scaled by N (e.g. `R128`) |
| `i` | Invert the color component |
| `b{N}` | Brightness scaled by N |

#### Operators

| Op | Description |
|----|-------------|
| `+` `-` `*` `/` `%` | Arithmetic |
| `#` | Power of |
| `&` `\|` `^` | Bitwise AND, OR, XOR |
| `:` | Bitwise AND NOT |
| `<` `>` | Bit shift left / right |
| `?` | 255 if left > right, else 0 |
| `@` | Weight left side in range 0-255 |

Parentheses `( )` for grouping, and literal numbers.

#### Examples

```
128 & (c - ((c - 150 + s) > 5 < s))
(c & (c ^ 55)) + 25
128 & (c + 255) : (s ^ (c ^ 255)) + 25
c ^ 128
Y + N % 50
H - L
```

## WebSocket API

Requires OBS WebSocket (built into OBS 28+). The plugin registers as vendor `"glitch"`.

All requests accept an optional `"source"` field to target a specific OBS source by name. Omit it to broadcast to all Glitch filter instances.

### Requests

**`set_expression`** -- permanently change the expression

```json
{
  "vendorName": "glitch",
  "requestType": "set_expression",
  "requestData": {
    "expression": "c ^ 128",
    "source": "Camera"
  }
}
```

**`pulse`** -- apply an expression for a duration then revert

```json
{
  "vendorName": "glitch",
  "requestType": "pulse",
  "requestData": {
    "expression": "c ^ 200",
    "duration_ms": 3000
  }
}
```

**`set_enabled`** -- enable or disable the filter

```json
{
  "vendorName": "glitch",
  "requestType": "set_enabled",
  "requestData": { "enabled": false }
}
```

**`get_state`** -- query current filter state

```json
{
  "vendorName": "glitch",
  "requestType": "get_state",
  "requestData": {}
}
```

Returns the current expression, enabled state, seed, frame count, and a comma-separated list of all active source names in the `sources` field.

### Go test client

A small Go client is included for testing:

```bash
cd examples/ws-client
go run . pulse "c ^ 128" 3000
go run . --source "Camera" set_expression "r & 0xF0"
go run . get_state
```

Set `OBS_WS_PASSWORD` if authentication is enabled.

## Building

### Requirements

- Rust 1.80+
- OBS Studio development headers (`libobs-dev` on Ubuntu/Arch)
- `pkg-config` (Linux)
- `cc` C compiler

### Build

```bash
cargo build --release
```

The output is `target/release/libobs_glitch.so` (Linux), `.dylib` (macOS), or `.dll` (Windows).

## License

GPLv2 -- see [LICENSE](LICENSE).
