python3 -c "
import math
s=0.0
for i in range(1,600000):
    s+=math.sqrt(i)*1.5+math.sin(i*0.001)/i
a=[float(i)*0.5 for i in range(20000)]
t=0.0
for r in range(10):
    t+=sum(x*x for x in a)
print('FP2_DONE',round(s,3),round(t,1))
"
awk 'BEGIN{s=0;for(i=1;i<400000;i++){s+=sqrt(i)*1.0001+i/7.0};printf "AWK_DONE %.2f\n",s}'
