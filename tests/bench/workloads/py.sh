python3 -c "
import zlib,hashlib
M=(1<<64)-1
x=88172645463325252
for i in range(300000):
    x^=(x<<13)&M; x^=x>>7; x^=(x<<17)&M
d=bytes(range(256))*2048
h=hashlib.sha256(d).hexdigest()
c=zlib.compress(d,6); zlib.decompress(c)
words=['alpha','beta','gamma','delta']*2000
s=' '.join(sorted(set(words)))+str(len(c))
print('PY_DONE',x&0xff,h[:8],len(c))
"
