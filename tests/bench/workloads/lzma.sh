cp /usr/bin/7zz /tmp/in.bin &&
    7z a -mmt1 -mx5 /tmp/t.7z /tmp/in.bin >/dev/null &&
    7z x -so /tmp/t.7z >/dev/null &&
    echo LZMA_DONE
