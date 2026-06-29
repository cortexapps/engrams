#!/usr/bin/env bash
# ADR 0027: bake-time self-containment smoke for the `playwright` RO bundle.
#
# Runs INSIDE build.sh's throwaway debian build container, against the staged
# bundle tree ($1). It guards the regression that shipped a browser which
# crashed on real pages: the ldd-walk in build.sh is blind to the NSS PKCS#11
# modules chromium dlopen's at runtime (softoken/freebl/built-in-roots) and to
# their own transitive deps (libsqlite3). A missing module aborts chromium
# FATAL in crypto/nss_util.cc the first time a page forces the NSS trust store.
#
# Two things make this a REAL guard rather than a false green:
#   1. chromium only forces the NSS path when a server cert chains to a
#      PRIVATELY-added root — i.e. the egress proxy's MITM CA in prod. Public
#      roots, http, about:blank all skip NSS. So we serve TLS with a private CA
#      and trust it via the user NSS DB chromium consults.
#   2. The build container itself ships system NSS, which would silently
#      satisfy chromium's dlopen and mask an incomplete bundle. We delete the
#      system NSS modules (+ libsqlite3) first, so the bundle's own lib/ is the
#      ONLY source — exactly like the minimal FC guest base image.
#
# Exit non-zero on any failure so build.sh's `set -e` aborts the bake.
set -euo pipefail

BUNDLE="${1:?usage: smoke.sh <bundle-root>}"
export DEBIAN_FRONTEND=noninteractive

SHELL_BIN="$(find "$BUNDLE/ms-playwright" -type f \( -name chrome-headless-shell -o -name headless_shell \) | head -n1)"
[ -n "$SHELL_BIN" ] || { echo "SMOKE FATAL: headless-shell binary not found" >&2; exit 1; }
NODE="$BUNDLE/node/bin/node"

# certutil (to populate the NSS DB) + openssl (to mint the CA). apt lists were
# wiped after the main install, so refresh them.
apt-get update -qq >/dev/null
apt-get install -y -qq --no-install-recommends libnss3-tools openssl >/dev/null

work="$(mktemp -d)"
export HOME="$work/home"
mkdir -p "$HOME"
cd "$work"

# 1) private CA + a server cert for 127.0.0.1 chained to it (the "proxy MITM").
openssl genrsa -out ca.key 2048 2>/dev/null
openssl req -x509 -new -key ca.key -days 1 -subj "/CN=engram-smoke-ca" -out ca.pem 2>/dev/null
openssl genrsa -out srv.key 2048 2>/dev/null
openssl req -new -key srv.key -subj "/CN=127.0.0.1" -out srv.csr 2>/dev/null
printf 'subjectAltName=IP:127.0.0.1\n' > ext.cnf
openssl x509 -req -in srv.csr -CA ca.pem -CAkey ca.key -CAcreateserial -days 1 \
    -out srv.pem -extfile ext.cnf 2>/dev/null

# 2) trust the CA in the user NSS DB chromium reads (created while system
#    softoken still exists; chromium reading it forces crypto::EnsureNSSInit).
mkdir -p "$HOME/.pki/nssdb"
certutil -N -d "sql:$HOME/.pki/nssdb" --empty-password
certutil -A -d "sql:$HOME/.pki/nssdb" -n engram-smoke-ca -t C,, -i ca.pem

# 3) make the staged bundle the ONLY NSS source — drop system modules + sqlite.
nssdir="$(dirname "$(find /usr/lib -name libsoftokn3.so 2>/dev/null | head -n1)")"
rm -f "$nssdir"/libsoftokn3.so "$nssdir"/libfreebl3.so "$nssdir"/libfreeblpriv3.so \
      "$nssdir"/libnssckbi.so "$nssdir"/libnssdbm3.so "$nssdir"/libsqlite3.so.0*

# 4) serve the private-root page and screenshot it with the bundle's chromium.
"$NODE" -e '
  const https=require("https"),fs=require("fs");
  https.createServer({key:fs.readFileSync("srv.key"),cert:fs.readFileSync("srv.pem")},
    (q,s)=>{s.setHeader("content-type","text/html");s.end("<h1>smoke ok</h1>")})
    .listen(8443,"127.0.0.1",()=>process.stdout.write("UP\n"));' &
srv_pid=$!
trap 'kill "$srv_pid" 2>/dev/null || true' EXIT
sleep 1

shot="$work/smoke.png"
if ! LD_LIBRARY_PATH="$BUNDLE/lib" "$SHELL_BIN" \
        --no-sandbox --headless=new --enable-logging=stderr --v=1 \
        --screenshot="$shot" "https://127.0.0.1:8443/" >"$work/chrome.log" 2>&1; then
    echo "SMOKE FAIL: bundle chromium crashed on a private-root HTTPS page (NSS path)." >&2
    grep -iaE 'nss_error|nss_util|cannot open|FATAL' "$work/chrome.log" | tail -5 >&2 || true
    echo "  -> the bundle's lib/ is missing an NSS module or one of its deps." >&2
    exit 1
fi
[ -s "$shot" ] || { echo "SMOKE FAIL: no screenshot produced (renderer never painted)." >&2; exit 1; }

echo "playwright bundle smoke OK — rendered a private-root HTTPS page using only bundled NSS ($(stat -c%s "$shot") bytes)"
