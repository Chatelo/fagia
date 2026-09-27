#!/usr/bin/env sh
# OS-specific code lives only in fagia-core/src/platform/.
set -eu
cd "$(dirname "$0")/.."
hits=$(grep -rn 'cfg(target_os' crates --include='*.rs' | grep -v '^crates/fagia-core/src/platform/' || true)
if [ -n "$hits" ]; then
  echo "cfg(target_os) outside src/platform/:" >&2
  echo "$hits" >&2
  exit 1
fi
# Only the action gate may allow the disallowed remove/signal methods.
allows=$(grep -rn 'allow(clippy::disallowed_methods)' crates --include='*.rs' \
  | grep -v '^crates/fagia-core/src/actions/' \
  | grep -v '^crates/fagia-core/src/platform/linux.rs' || true)
if [ -n "$allows" ]; then
  echo "disallowed_methods allowed outside the action gate:" >&2
  echo "$allows" >&2
  exit 1
fi
echo "platform boundary ok"
