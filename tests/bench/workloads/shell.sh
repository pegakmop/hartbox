set -e
for i in 1 2 3; do
    find /etc /usr/lib -type f | sort > /tmp/shell-files
    wc -l /tmp/shell-files
    tar -cf /dev/null /etc /usr/share 2>/dev/null
    grep -r -l 'include' /usr/include 2>/dev/null | sort | wc -l
    gzip -c /bin/busybox | gzip -d | sha256sum
    awk 'BEGIN {s=0; for(i=0;i<30000;i++) s+=(i*17)%101; print s}'
    apk info -v | sort | wc -l
    printf '%s\n' zeta alpha beta alpha | sort | uniq -c
 done
echo SHELL_DONE
