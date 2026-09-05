#!/usr/bin/env bash
set -euo pipefail

root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../../.." && pwd)"
"$root/crates/sophon-sdk/scripts/check-upstream-sync.sh"
tree="$(cargo tree --manifest-path "$root/Cargo.toml" --locked -p sophon-sdk -e normal --prefix none)"
if grep -Eq '^(xai-grok-pager|ratatui|crossterm) v' <<< "$tree"; then
  echo 'SDK normal dependency closure contains a TUI dependency' >&2
  exit 1
fi

# Inspect resolved, public rustdoc signatures rather than grepping Rust source:
# aliases, multiline declarations and public reexports must not hide ACP types.
# Without local dependency docs rustdoc can render an external type as bare
# text, hiding its origin. Build the forbidden-but-internal ACP docs first.
cargo doc --manifest-path "$root/Cargo.toml" --locked --no-deps \
  -p agent-client-protocol -p agent-client-protocol-schema -p xai-acp-lib
cargo rustdoc --manifest-path "$root/Cargo.toml" --locked -p sophon-sdk -- -D rustdoc::broken_intra_doc_links
target="$(cargo metadata --manifest-path "$root/Cargo.toml" --locked --no-deps --format-version 1 | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')"
python3 - "$target/doc/sophon_sdk" <<'PY'
import pathlib
import sys
from html.parser import HTMLParser
from urllib.parse import urlparse

FORBIDDEN = {"agent_client_protocol", "agent_client_protocol_schema", "xai_acp_lib", "xai_grok_pager", "ratatui", "crossterm"}


class Signatures(HTMLParser):
    def __init__(self):
        super().__init__()
        self.depth = 0
        self.signature = None
        self.blanket = None
        self.count = 0
        self.leaks = []

    def handle_starttag(self, tag, attrs):
        attrs = dict(attrs)
        if tag not in {"br", "hr", "img", "input", "meta", "link", "wbr", "source", "area", "base", "embed", "param", "track", "col"}:
            self.depth += 1
        # External `impl<T> Trait for T` is not an SDK declaration. Such traits
        # apply even to std types and cannot be removed by hiding SDK imports.
        if attrs.get("id") == "blanket-implementations-list":
            self.blanket = self.depth
        if self.blanket is not None:
            return
        if ({"item-decl", "code-header"} & set(attrs.get("class", "").split())
                or attrs.get("id", "").startswith("reexport.")):
            self.signature = self.depth
            self.count += 1
        if self.signature is not None and tag == "a":
            href = attrs.get("href", "")
            if FORBIDDEN & set(urlparse(href).path.split("/")):
                self.leaks.append(href)

    def handle_endtag(self, tag):
        if self.signature == self.depth:
            self.signature = None
        if self.blanket == self.depth:
            self.blanket = None
        self.depth -= 1


# Negative fixture: an innocent-looking alias still resolves to forbidden ACP.
probe = Signatures()
probe.feed('<pre class="rust item-decl"><code>pub type Alias =\n<a href="../agent_client_protocol/struct.SessionId.html">Alias</a>;</code></pre>')
assert probe.count == 1 and len(probe.leaks) == 1
probe = Signatures()
probe.feed('<dt id="reexport.Alias"><code>pub use <a href="../xai_acp_lib/struct.Sender.html">Alias</a>;</code></dt>')
assert probe.count == 1 and len(probe.leaks) == 1
probe = Signatures()
probe.feed('<div class="docblock"><a href="../agent_client_protocol/index.html">internal docs</a></div>')
assert not probe.leaks
probe = Signatures()
probe.feed('<div id="blanket-implementations-list"><h3 class="code-header"><a href="../agent_client_protocol_schema/trait.IntoOption.html">blanket</a></h3></div><h3 class="code-header"><a href="../agent_client_protocol/trait.Client.html">explicit impl</a></h3>')
assert probe.count == 1 and len(probe.leaks) == 1

count = 0
leaks = []
for path in pathlib.Path(sys.argv[1]).rglob("*.html"):
    parser = Signatures()
    parser.feed(path.read_text())
    count += parser.count
    leaks.extend((str(path), href) for href in parser.leaks)
if not count:
    raise SystemExit("No rustdoc signatures found; boundary check cannot validate this rustdoc format")
if leaks:
    raise SystemExit("Forbidden public signature links: " + repr(leaks))
print(f"PASS: no TUI dependencies; {count} SDK-declared public rustdoc signatures contain no ACP/TUI type links (dependency blanket impls excluded)")
PY
