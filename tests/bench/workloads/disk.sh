python3 - <<'PYR'
import mmap
import os
import tempfile
initial, path = tempfile.mkstemp(prefix='rvemu-io-', dir='/root')
os.close(initial)
chunk = 4096
pages = 10000
fd = os.open(path, os.O_RDWR | os.O_DIRECT)
buf = mmap.mmap(-1, chunk)
try:
    for i in range(pages):
        buf[:8] = i.to_bytes(8, 'little')
        assert os.pwritev(fd, [buf], i * chunk) == chunk
    os.fsync(fd)
    for k in range(pages):
        i = k * 3997 % pages
        assert os.preadv(fd, [buf], i * chunk) == chunk
        assert int.from_bytes(buf[:8], 'little') == i
finally:
    os.close(fd)
    os.unlink(path)
print('DISK_DONE')
PYR
