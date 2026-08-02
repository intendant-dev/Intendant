#!/usr/bin/env bash
# Symlink every skills-internal/<name> into the global agent-skills roots
# (~/.agents/skills and ~/.claude/skills). Dev-machine opt-in tooling only:
# the daemon and product code never run this and never embed these skills,
# and the daemon-side installer treats symlinks as user-owned (never
# followed, replaced, or swept) — so these links are invisible to product
# behavior. See skills-internal/README.md.
#
# Links always point at the MAIN checkout's skills-internal/, resolved via
# git's common dir even when this script runs from a worktree copy —
# landed changes go live through the link, and worktree paths would
# dangle once the worktree is reclaimed.
set -euo pipefail

script_dir="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
common_dir="$(git -C "$script_dir" rev-parse --git-common-dir)"
case "$common_dir" in
  /*) : ;;
  *) common_dir="$script_dir/$common_dir" ;;
esac
main_root="$(cd "$(dirname "$common_dir")" && pwd)"
src="$main_root/skills-internal"
if [ ! -d "$src" ]; then
  echo "error: $src not found (has skills-internal/ landed on the main checkout?)" >&2
  exit 1
fi

for root in "$HOME/.agents/skills" "$HOME/.claude/skills"; do
  mkdir -p "$root"
  # Prune links we own that no longer resolve (retired internal skills).
  for link in "$root"/*; do
    [ -L "$link" ] || continue
    case "$(readlink "$link")" in
      "$src"/*) [ -e "$link" ] || { rm "$link"; echo "pruned $link"; } ;;
    esac
  done
  for dir in "$src"/*/; do
    name="$(basename "$dir")"
    [ -f "$dir/SKILL.md" ] || continue
    dest="$root/$name"
    if [ -L "$dest" ]; then
      [ "$(readlink "$dest")" = "$src/$name" ] && continue
      rm "$dest"
    elif [ -e "$dest" ]; then
      echo "skip $dest: exists and is not a symlink (user-owned; remove it yourself to adopt the repo copy)" >&2
      continue
    fi
    ln -s "$src/$name" "$dest"
    echo "linked $dest -> $src/$name"
  done
done
echo "dev skills installed (symlinked from $src)"
