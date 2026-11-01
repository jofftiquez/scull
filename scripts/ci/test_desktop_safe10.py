#!/usr/bin/env python3
"""Self-test of desktop_safe10.py on synthetic `cargo metadata` graphs (no cargo needed)."""

import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import desktop_safe10 as d  # noqa: E402


def meta(edges: dict, renamed: dict | None = None) -> dict:
    """A metadata document: `edges` maps a package name to its normal dependencies' names."""
    pkgs = sorted({n for n in edges} | {x for xs in edges.values() for x in xs})
    nodes = []
    for n in pkgs:
        nodes.append(
            {
                "id": f"id-{n}",
                # `name` in deps is the key it was declared under; `pkg` is what counts.
                "deps": [
                    {"name": (renamed or {}).get(x, x), "pkg": f"id-{x}", "dep_kinds": [{"kind": None}]}
                    for x in edges.get(n, [])
                ],
            }
        )
    return {"packages": [{"id": f"id-{n}", "name": n} for n in pkgs], "resolve": {"nodes": nodes}}


def main() -> int:
    ok = {
        "scull-desktop": ["scull-desktop-bridge", "scull-platform"],
        "scull-desktop-bridge": ["scull-cleaner", "scull-mcp"],
        "scull-cleaner": ["scull-platform"],
        "scull-mcp": ["scull-core"],
    }
    names, deps = d.graph(meta(ok))
    assert d.dependents_of("scull-cleaner", names, deps) == {"scull-desktop-bridge"}
    assert not d.reaches("scull-mcp", "scull-cleaner", names, deps)

    # The app crate depending on the cleaner directly, under another key: caught.
    bad = dict(ok, **{"scull-desktop": ["scull-desktop-bridge", "scull-cleaner"]})
    names, deps = d.graph(meta(bad, renamed={"scull-cleaner": "deleter"}))
    assert d.dependents_of("scull-cleaner", names, deps) == {"scull-desktop", "scull-desktop-bridge"}

    # MCP reaching the cleaner through another crate: caught.
    leak = dict(ok, **{"scull-mcp": ["scull-proactive"], "scull-proactive": ["scull-cleaner"]})
    names, deps = d.graph(meta(leak))
    assert d.reaches("scull-mcp", "scull-cleaner", names, deps)

    # Dev-dependencies don't count.
    dev = meta(ok)
    for node in dev["resolve"]["nodes"]:
        if node["id"] == "id-scull-mcp":
            node["deps"].append({"name": "scull-cleaner", "pkg": "id-scull-cleaner", "dep_kinds": [{"kind": "dev"}]})
    names, deps = d.graph(dev)
    assert not d.reaches("scull-mcp", "scull-cleaner", names, deps)
    # bare-api: only a dev-dependency may ask for it.
    def pkg(features, kind):
        dep = {"name": "scull-cleaner", "features": features, "kind": kind}
        return {"packages": [{"name": "x", "dependencies": [dep]}]}

    assert d.bare_api_users(pkg(["bare-api"], "dev")) == set()
    assert d.bare_api_users(pkg([], None)) == set()
    assert d.bare_api_users(pkg(["bare-api"], None)) == {"x"}
    assert d.bare_api_users(pkg(["bare-api"], "build")) == {"x"}
    print("test_desktop_safe10: ok")
    return 0


if __name__ == "__main__":
    sys.exit(main())
