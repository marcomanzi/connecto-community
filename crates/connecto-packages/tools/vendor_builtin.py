#!/usr/bin/env python3
"""Regenerate crates/connecto-packages/builtin/ (the built-in `connecto` package).

Usage: vendor_builtin.py <extracted-tree> <out-dir>

The tree holds `.claude/`, `.codex/` and `.agents/` as the `both` variant has
them. `builtin/both` is where the content is edited, so to bring the other two
variants in line, write to a new folder and copy it over (the out-dir is
emptied first, so it must not be `builtin` itself):

  vendor_builtin.py builtin/both /tmp/regen && rsync -a --delete /tmp/regen/ builtin/

It writes three variants:

  both/    the content as it is (each side is reviewed by the other tool)
  claude/  `.claude` only; reviewers and critics run as Claude terminals
           (`connecto add claude`), and the Codex plugin is dropped from settings
  codex/   `.codex` and `.agents` only; reviewers and critics run as Codex

Bump `version` in each variant's package.json when the content changes in a release.
"""
import os, re, shutil, json, sys
src, out = sys.argv[1], sys.argv[2]
shutil.rmtree(out, ignore_errors=True)

def walk(base):
    for d, _, fs in os.walk(os.path.join(src, base)):
        for f in fs:
            if f == '.DS_Store': continue
            p = os.path.join(d, f)
            yield os.path.relpath(p, src)

def put(variant, rel, data):
    dest = os.path.join(out, variant, rel)
    os.makedirs(os.path.dirname(dest), exist_ok=True)
    mode = os.stat(os.path.join(src, rel)).st_mode if os.path.exists(os.path.join(src, rel)) else 0o644
    with open(dest, 'wb') as fh: fh.write(data)
    os.chmod(dest, mode & 0o755 | 0o644)

def swap(text, other, me):
    # other: the tool the original names as the reviewer; me: the tool to name instead.
    O, M = other, me
    ol, ml = O.lower(), M.lower()
    text = re.sub(r"don't substitute an? (Claude|Codex) review", "don't substitute your own review", text)
    text = text.replace("independent review by a different model", "independent review in a separate terminal")
    text = text.replace("so a different model reviews the work", "so a fresh, independent session reviews the work")
    text = text.replace("so a different model critiques the work", "so a fresh, independent session critiques the work")
    text = text.replace(f"{O} reviewers", f"{M} reviewers")
    text = text.replace(f"| {O}, in a Connecto terminal |", f"| {M}, in a Connecto terminal |")
    text = text.replace(f"separate {O} terminal", f"separate {M} terminal")
    text = text.replace(f"and {O} critiques the result", f"and {M} critiques the result")
    text = text.replace(f"connecto add {ol}", f"connecto add {ml}")
    # Reviewers and critics always run on the strong model of their own tool.
    strong = {"Codex": ("Codex `gpt-6.1-sol`", "gpt-6.1-sol"), "Claude": ("Claude Opus 5.5", "claude-opus-5-5")}
    text = text.replace(f"{strong[O][0]}, in a Connecto terminal", f"{strong[M][0]}, in a Connecto terminal")
    text = text.replace(strong[O][1], strong[M][1])
    text = text.replace(f"`{ol}-2 starting`", f"`{ml}-2 starting`")
    return text

def manifest(extra):
    return json.dumps({"name": "Connecto", "description": extra, "version": 1}, indent=2) + "\n"

# both: as it is
for rel in walk('.claude'): put('both', rel, open(os.path.join(src, rel), 'rb').read())
for rel in walk('.codex'): put('both', rel, open(os.path.join(src, rel), 'rb').read())
for rel in walk('.agents'): put('both', rel, open(os.path.join(src, rel), 'rb').read())
open(os.path.join(out, 'both', 'package.json'), 'w').write(manifest("Connecto's team workflows (dev, daily and brainstorm teams) for Claude and Codex: each side is reviewed by the other tool."))

# claude only
for rel in walk('.claude'):
    data = open(os.path.join(src, rel), 'rb').read()
    if rel == '.claude/settings.json':
        j = json.loads(data)
        j['enabledPlugins'].pop('codex@openai-codex', None)
        j['extraKnownMarketplaces'].pop('openai-codex', None)
        if not j['extraKnownMarketplaces']: j.pop('extraKnownMarketplaces')
        data = (json.dumps(j, indent=2) + "\n").encode()
    elif rel.endswith('.md'):
        data = swap(data.decode(), 'Codex', 'Claude').encode()
    put('claude', rel, data)
open(os.path.join(out, 'claude', 'package.json'), 'w').write(manifest("Connecto's team workflows for Claude only: reviewers and critics run as Claude terminals."))

# codex only
for base in ('.codex', '.agents'):
    for rel in walk(base):
        data = open(os.path.join(src, rel), 'rb').read()
        if rel.endswith('.md'):
            data = swap(data.decode(), 'Claude', 'Codex').encode()
        put('codex', rel, data)
open(os.path.join(out, 'codex', 'package.json'), 'w').write(manifest("Connecto's team workflows for Codex only: reviewers and critics run as Codex terminals."))
