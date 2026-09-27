import sys,subprocess,time,json,re,datetime
label,binary=sys.argv[1:3]
path=f'/tmp/ant-native-startup-{label}.log'
args=[binary,'--ipv4-only','-v','--bootstrap','207.148.94.42:10000','file','download','134e4537ad1b2e29f0dc48f8e025a560989e91055ebf1c66bca2208ca8bba889','-o',f'/tmp/ant-native-startup-{label}.bin']
p=subprocess.Popen(args,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
start=time.monotonic(); first=None; milestones=[]
try:
 with open(path,'w') as log:
  for line in p.stdout:
   log.write(line);log.flush()
   if first is None:
    try:first=datetime.datetime.fromisoformat(line.split()[0].replace('Z','+00:00'))
    except:pass
   match=re.search(r'Downloaded ([123])/147',line)
   if match:
    now=datetime.datetime.fromisoformat(line.split()[0].replace('Z','+00:00'))
    milestones.append({'chunk':int(match[1]),'seconds':(now-first).total_seconds()})
    if match[1]=='3':break
   if time.monotonic()-start>180:break
finally:
 p.terminate()
 try:p.wait(timeout=10)
 except subprocess.TimeoutExpired:p.kill();p.wait()
print(json.dumps({'variant':label,'milestones':milestones,'log':path}))
