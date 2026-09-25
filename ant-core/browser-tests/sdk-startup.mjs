import {createServer} from 'node:http';
import {readFile} from 'node:fs/promises';
import {resolve, sep} from 'node:path';
import {chromium} from 'playwright';
const [rootArg, runsArg='1', variant='candidate', goal='3'] = process.argv.slice(2);
if (!rootArg || !['3', 'full'].includes(goal) || !Number.isInteger(Number(runsArg)) || Number(runsArg) < 1) throw new Error('usage: sdk-startup.mjs <SDK dist directory> [runs] [label] [3|full]');
const limitSeconds = goal === 'full' ? 1800 : 180;
const root = resolve(rootArg);
const server=createServer(async (req,res)=>{
 const pathname=new URL(req.url,'http://localhost').pathname;
 if(pathname==='/'){res.setHeader('Content-Type','text/html');res.end('<!doctype html><title>Pooled startup measurement</title>');return;}
 const path=resolve(root,'.'+pathname);
 if(!path.startsWith(root+sep)){res.writeHead(403).end();return;}
 try{const bytes=await readFile(path);res.setHeader('Content-Type',path.endsWith('.wasm')?'application/wasm':'text/javascript');res.end(bytes);}
 catch(error){res.writeHead(404).end(String(error));}
});
await new Promise(resolve=>server.listen(0,'127.0.0.1',resolve));
const moduleUrl='/index.js';
const browser = await chromium.launch({headless:true});
try {
 for(let run=1;run<=Number(runsArg);run++) {
  const context=await browser.newContext();const page=await context.newPage();
  let finish;const done=new Promise(resolve=>{finish=resolve;});
  await page.exposeFunction('recordProbe',event=>{console.log(JSON.stringify({variant,run,...event}));if(event.kind==='error'){process.exitCode=1;finish();}else if(event.kind==='complete'||(goal==='3'&&event.message?.startsWith('Downloaded chunk 3/')))finish();});
  page.on('crash',()=>{console.log(JSON.stringify({variant,run,kind:'error',message:'browser page crashed'}));process.exitCode=1;finish();});
  await page.goto(`http://127.0.0.1:${server.address().port}/`);
  const running=page.evaluate(async ({moduleUrl,goal})=>{
   const {AutonomiClient}=await import(moduleUrl);const started=performance.now();let downloadStart,client;
   const report=event=>globalThis.recordProbe({seconds:(performance.now()-started)/1000,downloadSeconds:downloadStart===undefined?undefined:(performance.now()-downloadStart)/1000,...event});
   try {
    client=await AutonomiClient.connect({onProgress:e=>report({kind:'progress',message:e.message,status:e.status,phase:e.phase,completed:e.completed,total:e.total,unit:e.unit})});
    report({kind:'connected',peer:client.connection.bootstrap.peerId});
    downloadStart=performance.now();
    const address='134e4537ad1b2e29f0dc48f8e025a560989e91055ebf1c66bca2208ca8bba889';
    if(goal==='full'){
     const result=await client.download(address);
     await report({kind:'complete',address:result.file.address,bytes:result.bytes.byteLength,hash:result.hash,chunks:result.file.chunks.length});
    }else{
     await client.downloadAndSave(address,{useFilePicker:false});
     await report({kind:'complete'});
    }
   }catch(error){await report({kind:'error',message:String(error)});}finally{client?.close();}
  },{moduleUrl,goal}).catch(error=>{if(!page.isClosed()){console.log(JSON.stringify({variant,run,kind:'page-error',message:String(error)}));process.exitCode=1;}finish();});
  const timer=setTimeout(()=>{console.log(JSON.stringify({variant,run,kind:'limit',seconds:limitSeconds}));process.exitCode=1;finish();},limitSeconds*1000);
  await done;clearTimeout(timer);await context.close();await running;
 }
}finally{await browser.close();server.close();}
