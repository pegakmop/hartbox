set -e
find /usr/lib -type f | wc -l
tar -cf /dev/null /etc /usr/share 2>/dev/null
grep -rl "include" /usr/include 2>/dev/null | wc -l
hashes=$(sha256sum /usr/bin/7zz /bin/busybox)
printf '%s\n' "$hashes" | wc -l
echo MIX_DONE
