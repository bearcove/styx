# styx-py

Native Python parser for the [Styx configuration language](https://github.com/bearcove/styx).

## Installation

```bash
uv add styx
```

## Usage

```python
from styx import parse

doc = parse("""
name "My App"
version "1.0.0"
server {
    host localhost
    port 8080
}
""")

for entry in doc.entries:
    print(f"{entry.key} = {entry.value}")
```

## Development

```bash
# Install dev dependencies
uv sync --dev

# Run tests
uv run pytest

# Run linter
uv run ruff check .

# Run type checker
uv run mypy styx
```

## License

MIT
