cp /usr/bin/7zz /tmp/in.bin; gzip -6 -c /tmp/in.bin > /tmp/in.gz; gzip -dc /tmp/in.gz | wc -c; bzip2 -c /tmp/in.bin > /tmp/in.bz2; bzip2 -dc /tmp/in.bz2 | wc -c; echo GZ_DONE
