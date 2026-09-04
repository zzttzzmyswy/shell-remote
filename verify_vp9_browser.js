// 用真实浏览器 (Chromium WebCodecs) 解码 VP9 fMP4 流。
// localhost http server 提供页面与 /stream.bin → secure context → VideoDecoder 可用。
const http = require('http');
const fs = require('fs');
const { chromium } = require('playwright');

const HTML = `<!DOCTYPE html><html><head><meta charset="UTF-8"></head><body>
<canvas id="cv"></canvas><div id="log" style="font:12px monospace;white-space:pre;"></div>
<script>
const log = (s) => { document.getElementById('log').textContent += s + '\\n'; };
(async () => {
  const bytes = new Uint8Array(await (await fetch('/stream.bin')).arrayBuffer());
  let profile = 0, level = 10;
  for (let i = 0; i + 8 <= bytes.length; i++) {
    if (bytes[i]===0x76&&bytes[i+1]===0x70&&bytes[i+2]===0x63&&bytes[i+3]===0x43) {
      profile = bytes[i+8]; level = bytes[i+9]; break;
    }
  }
  const codec = 'vp09.'+String(profile).padStart(2,'0')+'.'+String(level).padStart(2,'0')+'.08';
  log('codec='+codec);
  const boxes=[]; let p=0;
  while (p+8<=bytes.length) {
    const size=(bytes[p]<<24)|(bytes[p+1]<<16)|(bytes[p+2]<<8)|bytes[p+3];
    if (size<8||p+size>bytes.length) break;
    const type=String.fromCharCode(bytes[p+4],bytes[p+5],bytes[p+6],bytes[p+7]);
    boxes.push({type,body:bytes.subarray(p+8,p+size)}); p+=size;
  }
  let decoded=0, keyframes=0, decErr='', nchunk=0, chunks=[];
  const canvas=document.getElementById('cv');
  const ctx=canvas.getContext('2d');
  if (typeof VideoDecoder === 'undefined') { log('VideoDecoder UNAVAILABLE'); return {decoded:0,err:'no-videodecoder'}; }
  const dec=new VideoDecoder({
    output(frame){ decoded++; canvas.width=frame.displayWidth; canvas.height=frame.displayHeight;
      ctx.drawImage(frame,0,0); frame.close(); },
    error(e){ decErr=e.message; log('decoder error: '+e.message); },
  });
  dec.configure({codec, optimizeForLatency:true});
  let pending=null;
  for (const bx of boxes) {
    if (bx.type==='moof') {
      let size=0,isKey=false,pts=0; const body=bx.body; let q=0;
      while (q+8<=body.length) {
        const sz=(body[q]<<24)|(body[q+1]<<16)|(body[q+2]<<8)|body[q+3]; if(sz<8)break;
        const tp=String.fromCharCode(body[q+4],body[q+5],body[q+6],body[q+7]);
        const inner=body.subarray(q+8,q+sz);
        if (tp==='traf') { let r=0;
          while (r+8<=inner.length) {
            const s2=(inner[r]<<24)|(inner[r+1]<<16)|(inner[r+2]<<8)|inner[r+3]; if(s2<8)break;
            const t2=String.fromCharCode(inner[r+4],inner[r+5],inner[r+6],inner[r+7]);
            const d2=inner.subarray(r+8,r+s2);
            if (t2==='tfdt'&&d2.length>=12) {
              pts=Number((BigInt(d2[4])<<56n)|(BigInt(d2[5])<<48n)|(BigInt(d2[6])<<40n)|
                (BigInt(d2[7])<<32n)|(BigInt(d2[8])<<24n)|(BigInt(d2[9])<<16n)|
                (BigInt(d2[10])<<8n)|BigInt(d2[11]));
            } else if (t2==='trun'&&d2.length>=16) {
              // first_sample_flags 高字节低 2 位 = sample_depends_on; 2=key, 1=delta。
              const sampleFlags = d2[12];
              isKey = ((sampleFlags & 0x03) === 0x02);
              size=(d2[d2.length-4]<<24)|(d2[d2.length-3]<<16)|(d2[d2.length-2]<<8)|d2[d2.length-1];
            }
            r+=s2;
          }
        }
        q+=sz;
      }
      pending={size,isKey,pts};
    } else if (bx.type==='mdat'&&pending) {
      const sample=bx.body.subarray(bx.body.length-pending.size);
      if (pending.isKey) keyframes++;
      nchunk++;
      chunks.push((pending.isKey?'K':'D')+':'+pending.size+'@'+pending.pts);
      try { dec.decode(new EncodedVideoChunk({type:'key',
        timestamp:pending.pts, data:sample})); } catch(e) { log('decode throw: '+e.message); }
      pending=null;
    }
  }
  try { await dec.flush(); } catch(e) { log('flush throw: '+e.message); }
  await new Promise(r=>setTimeout(r,400));
  return {decoded, keyframes, codec, decErr, nchunk, head:chunks.slice(0,8).join(' '),
    tail:chunks.slice(-3).join(' ')};
})().then(r=>window.__res=r);
</script></body></html>`;

const server = http.createServer((req, res) => {
  if (req.url === '/stream.bin') {
    res.writeHead(200, {'Content-Type':'application/octet-stream'});
    fs.createReadStream('/tmp/vp9_stream.bin').pipe(res);
  } else {
    res.writeHead(200, {'Content-Type':'text/html'});
    res.end(HTML);
  }
});

server.listen(0, '127.0.0.1', async () => {
  const port = server.address().port;
  const browser = await chromium.launch({headless:true, args:['--no-sandbox','--disable-gpu']});
  const page = await browser.newPage();
  const errs = [];
  page.on('pageerror', e => errs.push('pageerror: '+e.message));
  page.on('console', m => { if (m.type()==='error') errs.push('console: '+m.text()); });
  await page.goto('http://127.0.0.1:' + port + '/');
  // 等解码完成
  await page.waitForFunction('window.__res', null, {timeout: 15000});
  const res = await page.evaluate('window.__res');
  await browser.close();
  server.close();
  console.log('RESULT', JSON.stringify(res));
  if (errs.length) console.log('ERRORS', errs.join(' | '));
});