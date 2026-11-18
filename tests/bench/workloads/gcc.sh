awk 'BEGIN{for(i=0;i<150;i++){printf "int f%d(int a,int b){int s=0;for(int i=0;i<a;i++){s+=(i*b)^(s>>3);if(s&1)s+=i;}return s+%d;}\n",i,i} print "int main(){int t=0;"; for(i=0;i<150;i++) printf "t+=f%d(10,%d);\n",i,i; print "return t&1;}"}' > /tmp/g.c; if gcc -O2 -o /tmp/g /tmp/g.c; then
    status=0
    /tmp/g || status=$?
    if [ "$status" -le 1 ]; then echo GCC_DONE; else echo GCC_FAILED_$status; fi
fi
