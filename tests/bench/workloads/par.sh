awk 'BEGIN{for(i=0;i<60;i++){printf "int f%d(int a,int b){int s=0;for(int i=0;i<a;i++){s+=(i*b)^(s>>3);if(s&1)s+=i;}return s+%d;}\n",i,i} print "int main(){int t=0;"; for(i=0;i<60;i++) printf "t+=f%d(10,%d);\n",i,i; print "return t&1;}"}' > /tmp/p.c; for k in 1 2 3 4; do (gcc -O2 -o /tmp/p$k /tmp/p.c && /tmp/p$k; python3 -c 'x=1
for i in range(200000): x=(x*1103515245+12345)&0xffffffff
print("par",x)') & done; wait; echo PAR_DONE
