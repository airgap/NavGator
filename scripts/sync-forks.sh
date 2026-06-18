#!/usr/bin/env bash
# Maintained-fork sync tooling for swerve's engine forks (docs/FORK.md, ROADMAP §R2).
#
#   sync-forks.sh --check     Report how far each fork trails upstream (no clone; CI canary).
#   sync-forks.sh --merge     Merge upstream into each fork on the cadence (needs work clones).
#
# We never file upstream, but we DO merge from upstream so the forks don't rot.
set -euo pipefail

# fork repo (under airgap) | upstream repo | upstream ref to track | our pinned rev
FORKS=(
  "airgap/swervo|servo/servo|main|ed1af70e712aa7ae0df4611241f10f6204389b70"
  "airgap/stylo|servo/stylo|main|49e912cf401a7f867d33d61baa2389bb3d6f73e0"
  "airgap/webrender|servo/webrender|0.69|dcfd5424deab751280e34a39220a7e1562263294"
)

MODE="${1:---check}"
WORKDIR="${SWERVE_FORKS_DIR:-$HOME/swerve-forks}"

check() {
  local drift=0
  for entry in "${FORKS[@]}"; do
    IFS='|' read -r fork upstream ref ours <<<"$entry"
    # `ours` is an upstream commit, so compare it to the upstream ref via the API.
    local ahead
    ahead="$(gh api "repos/${upstream}/compare/${ours}...${ref}" --jq '.ahead_by' 2>/dev/null || echo '?')"
    if [ "$ahead" = "0" ]; then
      echo "✓ ${fork}: up to date with ${upstream}@${ref}"
    else
      echo "▲ ${fork}: ${ahead} commit(s) behind ${upstream}@${ref} (pinned ${ours:0:12})"
      drift=1
    fi
  done
  [ "$drift" = "0" ] && echo "All engine forks current." || echo "Engine forks are behind upstream — run --merge."
  return 0
}

merge() {
  mkdir -p "$WORKDIR"
  for entry in "${FORKS[@]}"; do
    IFS='|' read -r fork upstream ref _ours <<<"$entry"
    local name="${fork#airgap/}" dir="$WORKDIR/${fork#airgap/}"
    echo "=== ${fork} <- ${upstream}@${ref} ==="
    if [ ! -d "$dir/.git" ]; then
      git clone "git@github.com:${fork}.git" "$dir"
    fi
    git -C "$dir" remote get-url upstream >/dev/null 2>&1 || \
      git -C "$dir" remote add upstream "https://github.com/${upstream}.git"
    git -C "$dir" fetch upstream "$ref"
    # Our integration branch is the fork's default branch (main, or 0.69 for webrender).
    local our_branch; our_branch="$(git -C "$dir" symbolic-ref --short HEAD)"
    git -C "$dir" merge --no-edit "upstream/${ref}" || {
      echo "!! merge conflict in ${name}: resolve in $dir (our patches vs upstream), commit, then push." >&2
      exit 1
    }
    git -C "$dir" push origin "$our_branch"
    echo "Merged + pushed ${name}. New HEAD: $(git -C "$dir" rev-parse --short HEAD)"
  done
  echo
  echo "Next: bump the pinned revs in Cargo.toml (swervo: crates/swerve-engine; stylo/webrender:"
  echo "root [patch] tables) to the new HEADs, run 'cargo update', and let CI go green before committing."
}

case "$MODE" in
  --check) check ;;
  --merge) merge ;;
  *) echo "usage: $0 [--check|--merge]" >&2; exit 2 ;;
esac
