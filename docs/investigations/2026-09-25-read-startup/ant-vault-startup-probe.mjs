import { chromium } from '../../../ant-core/browser-tests/node_modules/playwright/index.mjs';
const browser = await chromium.launch({headless:true});
try {
 const page = await browser.newPage();
 let finish;
 const done = new Promise(resolve=>finish=resolve);
 await page.exposeFunction('recordProbe', event=>{console.log(JSON.stringify(event));if(event.message?.startsWith('Downloaded chunk 3/')||event.status==='failed')finish();});
 await page.route('**/startup-probe.html',route=>route.fulfill({contentType:'text/html',body:'<!doctype html><title>Isolated SDK startup probe</title>'}));
 await page.goto('http://127.0.0.1:5180/startup-probe.html');
 const run = page.evaluate(async()=>{
  const {AutonomiClient}=await import('/@fs/Users/mick/RustroverProjects/ant-client-browser-sdk/dist/index.js');
  const t=performance.now();let downloadStart;
  const report=e=>globalThis.recordProbe({s:+((performance.now()-t)/1000).toFixed(3),downloadS:downloadStart===undefined?undefined:+((performance.now()-downloadStart)/1000).toFixed(3),operation:e.operation,status:e.status,message:e.message});
  const client=await AutonomiClient.connect('/ip4/207.148.94.42/udp/10001/webrtc-direct/certhash/uEiAkAsFZc-iV5UToJHyETA8GLBIVdzB10Qv6VuNlwqTwkQ/p2p/57a89f84e1652d22fd6b791c29d83f187fbed3ba8e08e265fe3031b3a6f725da',{onProgress:report});
  try{downloadStart=performance.now();await client.downloadAndSave('134e4537ad1b2e29f0dc48f8e025a560989e91055ebf1c66bca2208ca8bba889',{useFilePicker:false});}
  finally{client.close();}
 }).catch(error=>{console.log(String(error));finish();});
 const timer=setTimeout(()=>{console.log('Probe limit: 180 seconds');finish();},180000);
 await done;clearTimeout(timer);
 await page.close();await run;
}finally{await browser.close();}
