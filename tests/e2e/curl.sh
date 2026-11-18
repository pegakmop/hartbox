#!/bin/sh
# Test address fallback, IPv6 loopback, concurrent TCP and a streamed upload.
# ROOT_IMG must be a disposable Alpine root image with curl installed.
set -eu
: "${ROOT_IMG:?Set ROOT_IMG to a disposable Alpine image with curl installed}"
here=$(cd "$(dirname "$0")" && pwd)
rvemu=${RVEMU:-$here/../../target/release/rvemu}
out=$(mktemp -d)
server=
cleanup() {
    if [ -n "$server" ]; then
        kill "$server" 2>/dev/null || true
        wait "$server" 2>/dev/null || true
    fi
    rm -rf "$out"
}
trap cleanup EXIT

python3 - "$out/ports" <<'PY' &
import hashlib
import http.server
from pathlib import Path
import socket
import sys
import threading


class Handler(http.server.BaseHTTPRequestHandler):
    def reply(self, result):
        self.send_response(200)
        self.send_header("Content-Length", str(len(result)))
        self.end_headers()
        self.wfile.write(result)

    def do_GET(self):
        self.reply(b"rvemu-loopback")

    def do_POST(self):
        remaining = int(self.headers["Content-Length"])
        digest = hashlib.sha256()
        while remaining:
            chunk = self.rfile.read(min(16384, remaining))
            if not chunk:
                return
            digest.update(chunk)
            remaining -= len(chunk)
        self.reply(digest.hexdigest().encode())


class IPv6Server(http.server.ThreadingHTTPServer):
    address_family = socket.AF_INET6


with http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler) as v4:
    with IPv6Server(("::1", 0), Handler) as v6:
        # The IPv4 port must not have a listener on ::1 for the fallback test.
        assert v4.server_port != v6.server_port
        threading.Thread(target=v6.serve_forever, daemon=True).start()
        ports = Path(sys.argv[1])
        ready = ports.with_suffix(".ready")
        ready.write_text(f"{v4.server_port} {v6.server_port}\n")
        ready.rename(ports)
        v4.serve_forever()
PY
server=$!
for _ in 1 2 3 4 5; do
    [ -s "$out/ports" ] && break
    sleep 1
done
read -r v4 v6 < "$out/ports"

commands=$(cat <<EOF
set -e
test "\$(curl -fsS --noproxy '*' --max-time 12 --resolve 'fallback.invalid:$v4:[fd00:2::2],10.0.2.2' http://fallback.invalid:$v4/)" = rvemu-loopback
echo CURL_FALLBACK_OK
test "\$(curl -6 -fsS --noproxy '*' --max-time 8 'http://[fd00:2::2]:$v6/')" = rvemu-loopback
echo CURL_IPV6_OK
pids=
for n in 1 2 3 4; do
    (test "\$(curl -fsS --noproxy '*' --max-time 8 http://10.0.2.2:$v4/)" = rvemu-loopback) &
    pids="\$pids \$!"
done
for pid in \$pids; do
    wait "\$pid"
done
echo CURL_PARALLEL_OK
dd if=/dev/zero of=/tmp/rvemu-upload bs=65536 count=32 2>/dev/null
sum=\$(sha256sum /tmp/rvemu-upload | cut -d' ' -f1)
test "\$(curl -fsS --noproxy '*' --max-time 12 --data-binary @/tmp/rvemu-upload http://10.0.2.2:$v4/)" = "\$sum"
echo CURL_UPLOAD_OK
EOF
)
timeout 35 "$here/../bench/run.sh" "$rvemu" "$commands" --net > "$out/guest.log" 2>&1 || true
for marker in CURL_FALLBACK_OK CURL_IPV6_OK CURL_PARALLEL_OK CURL_UPLOAD_OK; do
    if ! grep -q "^$marker$" "$out/guest.log"; then
        cat "$out/guest.log"
        exit 1
    fi
    printf '%s\n' "$marker"
done
