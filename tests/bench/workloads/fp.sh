cat > /tmp/f.c <<'EOC'
#include <stdio.h>
#include <math.h>
#define N 256
static double A[N][N], B[N][N], C[N][N];
static float X[4096], Y[4096];
int main(void){
  for(int i=0;i<N;i++)for(int j=0;j<N;j++){A[i][j]=i*0.5+j;B[i][j]=i-j*0.25;}
  for(int i=0;i<N;i++)for(int k=0;k<N;k++){double a=A[i][k];for(int j=0;j<N;j++)C[i][j]+=a*B[k][j];}
  double s=0;for(int i=0;i<N;i++)s+=C[i][i];
  for(int i=0;i<4096;i++){X[i]=i*0.001f;Y[i]=1.0f;}
  float t=0;for(int r=0;r<200;r++)for(int i=0;i<4096;i++){t+=sqrtf(X[i]*X[i]+Y[i])/(Y[i]+1.5f);}
  double u=0;for(int i=1;i<400000;i++){u+=sin(i*0.001)*cos(i*0.002)+(double)(int)(i*0.37);}
  printf("FP_DONE %f %f %f\n",s,t,u);return 0;}
EOC
gcc -O2 -o /tmp/f /tmp/f.c -lm && /tmp/f; echo FP_END
